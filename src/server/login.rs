use std::sync::Arc;

use async_trait::async_trait;
use hyper::{Body, Request, Response, StatusCode};
use log::{error, info, warn};
use ring::{digest, pbkdf2};
use rustc_serialize::hex::{FromHex, ToHex};
use serde_derive::Deserialize;
use serde_json::json;

use crate::config::Config;
use crate::ldap_auth;
use crate::server::http::{parse_json, Filter, FilterResult, Handler};
use crate::server::sessions::Sessions;
use crate::util;

static DIGEST_ALG: &'static digest::Algorithm = &digest::SHA256;
const CREDENTIAL_LEN: usize = digest::SHA256_OUTPUT_LEN;

fn pbdkf2_iterations() -> std::num::NonZeroU32 {
    std::num::NonZeroU32::new(100_000).unwrap()
}

pub fn store_password(pass: &str, salt: &str) -> String {
    let mut pass_hash = [0u8; CREDENTIAL_LEN];
    pbkdf2::derive(
        DIGEST_ALG,
        pbdkf2_iterations(),
        salt.as_bytes(),
        pass.as_bytes(),
        &mut pass_hash,
    );

    pass_hash.to_hex()
}

pub fn verify_password(pass: &str, salt: &str, pass_hash: &str) -> bool {
    let pass_hash = match pass_hash.from_hex() {
        Ok(h) => h,
        Err(e) => {
            error!("Invalid password hash stored: {} -- {}", pass_hash, e);
            return false;
        }
    };
    pbkdf2::verify(
        DIGEST_ALG,
        pbdkf2_iterations(),
        salt.as_bytes(),
        pass.as_bytes(),
        &pass_hash,
    )
    .is_ok()
}

pub struct LoginHandler {
    sessions: Arc<Sessions>,
    config: Arc<Config>,
}

pub struct LogoutHandler {
    sessions: Arc<Sessions>,
}

pub struct SessionCheckHandler {
    sessions: Arc<Sessions>,
}

pub struct LoginSessionFilter {
    sessions: Arc<Sessions>,
}

impl LoginHandler {
    pub fn new(sessions: Arc<Sessions>, config: Arc<Config>) -> Box<LoginHandler> {
        Box::new(LoginHandler {
            sessions: sessions,
            config: config,
        })
    }
}

impl LogoutHandler {
    pub fn new(sessions: Arc<Sessions>) -> Box<LogoutHandler> {
        Box::new(LogoutHandler { sessions: sessions })
    }
}

impl SessionCheckHandler {
    pub fn new(sessions: Arc<Sessions>) -> Box<SessionCheckHandler> {
        Box::new(SessionCheckHandler { sessions: sessions })
    }
}

impl LoginSessionFilter {
    pub fn new(sessions: Arc<Sessions>) -> Box<LoginSessionFilter> {
        Box::new(LoginSessionFilter { sessions: sessions })
    }
}

#[derive(Deserialize, Clone)]
struct LoginRequest {
    username: String,
    password: String,
}

fn get_session(req: &Request<Body>) -> Option<String> {
    req.headers()
        .get("session")
        .map(|h| String::from_utf8_lossy(h.as_bytes()).into_owned())
}

#[async_trait]
impl Handler for LoginHandler {
    async fn handle(&self, req: Request<Body>) -> Response<Body> {
        let config = self.config.clone();
        let sessions = self.sessions.clone();

        parse_json(req, move |login_req: LoginRequest| {
            let mut success = None;
            if let Some(ref admin) = config.admin {
                if admin.name == login_req.username {
                    if verify_password(&login_req.password, &admin.salt, &admin.pass_hash) {
                        info!("Admin auth success");
                        success = Some(true);
                    } else {
                        warn!("Admin auth failure");
                        success = Some(false);
                    }
                }
            }

            if success.is_none() {
                if let Some(ref ldap) = config.ldap {
                    match ldap_auth::auth(&login_req.username, &login_req.password, ldap) {
                        Ok(true) => {
                            info!("LDAP auth successfor user: {}", login_req.username);
                            success = Some(true)
                        }
                        Ok(false) => warn!("LDAP auth failure for user: {}", login_req.username),
                        Err(e) => error!("Error authenticating to LDAP: {}", e),
                    };
                }
            }

            if success == Some(true) {
                let sess_id = sessions.new_session();
                let json = json!({
                    "session": sess_id,
                });

                util::new_json_resp(json.to_string())
            } else {
                util::new_empty_resp(StatusCode::UNAUTHORIZED)
            }
        })
    }
}

fn invalid_session() -> Response<Body> {
    util::new_msg_resp(StatusCode::FORBIDDEN, "Invalid session")
}

#[async_trait]
impl Handler for LogoutHandler {
    async fn handle(&self, req: Request<Body>) -> Response<Body> {
        let sess: String = match get_session(&req) {
            Some(s) => s.to_string(),
            None => return invalid_session(),
        };

        self.sessions.remove_session(&sess);
        util::new_json_resp("{}".into())
    }
}

#[async_trait]
impl Handler for SessionCheckHandler {
    async fn handle(&self, req: Request<Body>) -> Response<Body> {
        let sess: String = match get_session(&req) {
            Some(s) => s.to_string(),
            None => return invalid_session(),
        };

        if self.sessions.is_valid_session(&sess) {
            self.respond_with(StatusCode::OK, "")
        } else {
            invalid_session()
        }
    }
}

impl Filter for LoginSessionFilter {
    fn filter(&self, req: &Request<Body>) -> FilterResult {
        let sess: String = match get_session(&req) {
            Some(s) => s.to_string(),
            None => return FilterResult::Halt(invalid_session()),
        };

        if self.sessions.is_valid_session(&sess) {
            FilterResult::Continue
        } else {
            FilterResult::Halt(invalid_session())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_password() {
        let pw_hash = store_password("the-pass", "some-salt");
        assert_eq!(true, verify_password("the-pass", "some-salt", &pw_hash));
        assert_eq!(false, verify_password("wrong-pass", "some-salt", &pw_hash));
        assert_eq!(false, verify_password("the-pass", "wrong-salt", &pw_hash));
    }
}
