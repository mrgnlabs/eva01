use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

use log::{debug, error, info};
use tiny_http::{Response, Server};

const HTTP_STATUS_OK: u16 = 200;
const HTTP_STATUS_NOT_FOUND: u16 = 404;
const HTTP_STATUS_SERVICE_UNAVAILABLE: u16 = 503;
const HTTP_STATUS_INTERNAL_SERVER_ERROR: u16 = 500;

#[derive(Debug)]
pub enum HealthState {
    Initializing,
    Running,
}

impl HealthState {
    fn to_http_response(&self) -> Response<std::io::Cursor<Vec<u8>>> {
        match self {
            HealthState::Running => Response::from_string("OK").with_status_code(HTTP_STATUS_OK),
            HealthState::Initializing => Response::from_string("Initializing...")
                .with_status_code(HTTP_STATUS_SERVICE_UNAVAILABLE),
        }
    }
}

pub struct HealthCheckServer {
    port: u16,
    health_state: Arc<Mutex<HealthState>>,
    stop_liquidator: Arc<AtomicBool>,
}

impl HealthCheckServer {
    pub fn new(
        port: u16,
        health_state: Arc<Mutex<HealthState>>,
        stop_liquidator: Arc<AtomicBool>,
    ) -> Self {
        HealthCheckServer {
            port,
            health_state,
            stop_liquidator,
        }
    }

    fn state_to_http_response(&self) -> Response<std::io::Cursor<Vec<u8>>> {
        if let Ok(state) = self.health_state.lock() {
            state.to_http_response()
        } else {
            error!("Failed to acquire lock to get health state");
            Response::from_string("Internal Server Error")
                .with_status_code(HTTP_STATUS_INTERNAL_SERVER_ERROR)
        }
    }

    pub fn start(&self) -> anyhow::Result<()> {
        info!("Running the Healthcheck server on port {}", self.port);

        let server = Server::http(format!("0.0.0.0:{}", self.port))
            .map_err(|e| anyhow::anyhow!("Failed to create the Healthcheck server: {}", e))?;

        while !self.stop_liquidator.load(Ordering::Relaxed) {
            for request in server.incoming_requests() {
                let response = match request.url() {
                    "/healthz" => {
                        debug!("Received the '/healthz' request");
                        self.state_to_http_response()
                    }
                    "/readyz" => {
                        debug!("Received the '/readyz' request");
                        self.state_to_http_response()
                    }
                    _ => Response::from_string("Not Found").with_status_code(HTTP_STATUS_NOT_FOUND),
                };
                if let Err(e) = request.respond(response) {
                    info!("Failed to respond to the unknown request: {}", e);
                }
            }
        }

        info!("Healthcheck server stopped");

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{atomic::AtomicBool, Arc, Mutex};

    #[test]
    fn test_health_state_to_http_response_running() {
        let resp = HealthState::Running.to_http_response();
        assert_eq!(resp.status_code(), HTTP_STATUS_OK);
    }

    #[test]
    fn test_health_state_to_http_response_initializing() {
        let resp = HealthState::Initializing.to_http_response();
        assert_eq!(resp.status_code(), HTTP_STATUS_SERVICE_UNAVAILABLE);
    }

    #[test]
    fn test_health_server_state_to_http_response_running() {
        let health_state = Arc::new(Mutex::new(HealthState::Running));
        let stop_liquidator = Arc::new(AtomicBool::new(false));
        let server = HealthCheckServer::new(8080, health_state, stop_liquidator);
        let resp = server.state_to_http_response();
        assert_eq!(resp.status_code(), HTTP_STATUS_OK);
    }

    #[test]
    fn test_health_server_state_to_http_response_initializing() {
        let health_state = Arc::new(Mutex::new(HealthState::Initializing));
        let stop_liquidator = Arc::new(AtomicBool::new(false));
        let server = HealthCheckServer::new(8080, health_state, stop_liquidator);
        let resp = server.state_to_http_response();
        assert_eq!(resp.status_code(), HTTP_STATUS_SERVICE_UNAVAILABLE);
    }

    #[test]
    fn test_health_server_new() {
        let health_state = Arc::new(Mutex::new(HealthState::Running));
        let stop_liquidator = Arc::new(AtomicBool::new(false));
        let server = HealthCheckServer::new(1234, health_state.clone(), stop_liquidator.clone());
        assert_eq!(server.port, 1234);
        assert!(Arc::ptr_eq(&server.health_state, &health_state));
        assert!(Arc::ptr_eq(&server.stop_liquidator, &stop_liquidator));
    }

    // Integration test for start() would require spinning up the server and making HTTP requests.
    // This is omitted for brevity and because it requires network access and async code.
}
