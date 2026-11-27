//! This module provides a minimal GitHub API stand-in for tests. It listens on
//! a local port, records every request received and replies with the canned
//! responses provided, which allows checking what the API wrappers actually
//! send over the wire.

use std::{
    collections::HashMap,
    fmt::Write as _,
    io::{BufRead, BufReader, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use octorust::{Client, auth::Credentials};
#[cfg(test)]
use openssl::rsa::Rsa;
use reqwest::Url;
use reqwest_middleware::ClientBuilder;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::{ClientPool, RateLimitPolicy};

/// Generate an RSA private key in PEM format for GitHub app credentials, so
/// that no key needs to be stored in the repository.
#[cfg(test)]
pub(crate) fn generate_app_private_key() -> String {
    let key = Rsa::generate(2048).expect("key to be generated");
    String::from_utf8(key.private_key_to_pem().expect("key to be encoded")).expect("pem to be valid utf-8")
}

/// GitHub API stand-in. The server thread runs until the test process exits.
pub struct FakeGitHub {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<Request>>>,
}

impl FakeGitHub {
    /// Start a new server that replies using the routes provided. Any other
    /// request is recorded too, and receives a 404 response.
    #[must_use]
    pub fn start(routes: Vec<Route>) -> Self {
        // Bind to a free local port
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener to bind");
        let addr = listener.local_addr().expect("listener to have an address");

        // Serve requests in the background, handling each connection on its
        // own thread so delayed routes do not serialize concurrent clients
        let requests = Arc::new(Mutex::new(vec![]));
        let recorded_requests = requests.clone();
        let routes = Arc::new(Mutex::new(routes));
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let routes = routes.clone();
                let recorded_requests = recorded_requests.clone();
                thread::spawn(move || {
                    serve(stream, &routes, &recorded_requests);
                });
            }
        });

        Self { addr, requests }
    }

    /// Return the base URL of this fake server.
    #[must_use]
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Return a GitHub API client that sends all requests to this server.
    #[must_use]
    pub fn client(&self) -> Client {
        // Skip any proxy set in the environment, as it wouldn't reach this server
        let http = reqwest::Client::builder().no_proxy().build().expect("http client to be built");

        // Setup GitHub API client
        let credentials = Credentials::Token("token".to_string());
        let mut client = Client::custom("clowarden-tests", credentials, ClientBuilder::new(http).build());
        client.with_host_override(self.base_url());
        client
    }

    /// Return a [`ClientPool`] configured with the production middleware and
    /// rate-limit governor, sending all requests to this server.
    #[must_use]
    pub fn client_pool(&self, policy: RateLimitPolicy) -> Arc<ClientPool> {
        self.client_pool_with_shutdown(policy, CancellationToken::new())
    }

    /// Return a [`ClientPool`] configured with the production middleware,
    /// rate-limit governor, and explicit shutdown cancellation token.
    #[must_use]
    pub fn client_pool_with_shutdown(
        &self,
        policy: RateLimitPolicy,
        shutdown_token: CancellationToken,
    ) -> Arc<ClientPool> {
        Arc::new(ClientPool::new_for_testing(
            self.base_url(),
            policy,
            shutdown_token,
        ))
    }

    /// Return the requests received so far, in the order they arrived.
    #[must_use]
    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().expect("lock not to be poisoned").clone()
    }
}

/// Request received by the server.
#[derive(Clone, Debug)]
pub struct Request {
    /// Request headers keyed by lowercased header name.
    pub headers: HashMap<String, String>,
    /// HTTP method.
    pub method: String,
    /// Path of the request target.
    pub path: String,
    /// Query parameters of the request target.
    pub query: HashMap<String, String>,
    /// Monotonic timestamp when the server received this request.
    pub received_at: Instant,

    /// JSON body, when the request has one.
    pub body: Option<Value>,
}

impl Request {
    /// Create a new request instance without query parameters.
    #[must_use]
    pub fn new(method: &str, path: &str, body: Option<Value>) -> Self {
        Self {
            headers: HashMap::new(),
            method: method.to_string(),
            path: path.to_string(),
            query: HashMap::new(),
            received_at: Instant::now(),
            body,
        }
    }

    /// Return the value of the header provided (matched case-insensitively).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(&name.to_ascii_lowercase()).map(String::as_str)
    }

    /// Return the value of the query parameter provided.
    pub fn query_param(&self, name: &str) -> Option<&str> {
        self.query.get(name).map(String::as_str)
    }
}

impl PartialEq for Request {
    fn eq(&self, other: &Self) -> bool {
        self.method == other.method
            && self.path == other.path
            && self.query == other.query
            && self.body == other.body
    }
}

/// Canned response for the requests matching a method and path.
#[derive(Clone)]
pub struct Route {
    /// Response body.
    body: String,
    /// Additional response headers.
    headers: Vec<(String, String)>,
    /// HTTP method matched.
    method: String,
    /// When true, this route is consumed on its first match.
    once: bool,
    /// Request path matched.
    path: String,
    /// Response status code.
    status: u16,

    /// Optional response delay simulating slow upstream endpoints.
    delay: Option<Duration>,
    /// Optional query parameter `(name, value)` filter.
    query_match: Option<(String, String)>,
}

impl Route {
    /// Create a new reusable route instance.
    pub fn new(method: &str, path: &str, status: u16, body: impl Into<String>) -> Self {
        Self {
            body: body.into(),
            headers: vec![],
            method: method.to_string(),
            once: false,
            path: path.to_string(),
            status,
            delay: None,
            query_match: None,
        }
    }

    /// Create a route that is consumed on its first match, allowing sequenced
    /// responses for retries.
    pub fn once(method: &str, path: &str, status: u16, body: impl Into<String>) -> Self {
        Self {
            once: true,
            ..Self::new(method, path, status, body)
        }
    }

    /// Delay sending the response for this route by `delay`.
    #[must_use]
    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = Some(delay);
        self
    }

    /// Add an HTTP response header to this route.
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Restrict this route to requests containing the query parameter given.
    #[must_use]
    pub fn with_query_param(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.query_match = Some((name.into(), value.into()));
        self
    }

    /// Check whether this route matches the incoming request.
    fn matches(&self, request: &Request) -> bool {
        if self.method != request.method || self.path != request.path {
            return false;
        }
        if let Some((key, value)) = &self.query_match {
            return request.query_param(key) == Some(value.as_str());
        }
        true
    }
}

/// Read a request from the stream provided.
fn read_request(reader: &mut impl BufRead) -> Request {
    // Read request line
    let mut request_line = String::new();
    reader.read_line(&mut request_line).expect("request line to be read");
    let mut request_line_parts = request_line.split_whitespace();
    let method = request_line_parts.next().expect("request to have a method").to_string();
    let target = request_line_parts.next().expect("request to have a target");

    // Read headers
    let mut content_length = 0;
    let mut headers = HashMap::new();
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).expect("header to be read");
        if header.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            let trimmed_value = value.trim();
            if name.eq_ignore_ascii_case("content-length") {
                content_length = trimmed_value.parse().expect("content length to be valid");
            }
            headers.insert(name.trim().to_ascii_lowercase(), trimmed_value.to_string());
        }
    }

    // Read body
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body).expect("body to be read");

    // Parse request target
    let url = Url::parse(&format!("http://localhost{target}")).expect("target to be valid");
    Request {
        headers,
        method,
        path: url.path().to_string(),
        query: url.query_pairs().into_owned().collect(),
        received_at: Instant::now(),
        body: serde_json::from_slice(&body).ok(),
    }
}

/// Serve the request available in the stream provided, closing the connection
/// after replying to it.
fn serve(mut stream: TcpStream, routes: &Mutex<Vec<Route>>, requests: &Mutex<Vec<Request>>) {
    // Read and record request
    let request = read_request(&mut BufReader::new(&stream));
    let matched = {
        let mut guard = routes.lock().expect("routes lock not to be poisoned");
        if let Some(index) = guard.iter().position(|r| r.matches(&request)) {
            if guard[index].once {
                Some(guard.remove(index))
            } else {
                Some(guard[index].clone())
            }
        } else {
            None
        }
    };
    requests.lock().expect("lock not to be poisoned").push(request);

    // Honor optional route delay before replying
    if let Some(delay) = matched.as_ref().and_then(|r| r.delay) {
        thread::sleep(delay);
    }

    // Send response
    let (status, headers, body) =
        matched.map_or((404, vec![], String::new()), |r| (r.status, r.headers, r.body));
    let mut response = format!(
        "HTTP/1.1 {status} \r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        let _ = write!(&mut response, "{name}: {value}\r\n");
    }
    response.push_str("\r\n");
    response.push_str(&body);
    let _ = stream.write_all(response.as_bytes());
}
