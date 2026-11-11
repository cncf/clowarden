//! This module provides a minimal GitHub API stand-in for tests. It listens on
//! a local port, records every request received and replies with the canned
//! responses provided, which allows checking what the API wrappers actually
//! send over the wire.

use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{Arc, Mutex},
    thread,
};

use octorust::{Client, auth::Credentials};
#[cfg(test)]
use openssl::rsa::Rsa;
use reqwest::Url;
use reqwest_middleware::ClientBuilder;
use serde_json::Value;

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

        // Serve requests in the background
        let requests = Arc::new(Mutex::new(vec![]));
        let recorded_requests = requests.clone();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                serve(stream, &routes, &recorded_requests);
            }
        });

        Self { addr, requests }
    }

    /// Return a GitHub API client that sends all requests to this server.
    #[must_use]
    pub fn client(&self) -> Client {
        // Skip any proxy set in the environment, as it wouldn't reach this server
        let http = reqwest::Client::builder().no_proxy().build().expect("http client to be built");

        // Setup GitHub API client
        let credentials = Credentials::Token("token".to_string());
        let mut client = Client::custom("clowarden-tests", credentials, ClientBuilder::new(http).build());
        client.with_host_override(format!("http://{}", self.addr));
        client
    }

    /// Return the requests received so far, in the order they arrived.
    #[must_use]
    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().expect("lock not to be poisoned").clone()
    }
}

/// Request received by the server.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    /// HTTP method.
    pub method: String,
    /// Path of the request target.
    pub path: String,
    /// Query parameters of the request target.
    pub query: HashMap<String, String>,

    /// JSON body, when the request has one.
    pub body: Option<Value>,
}

impl Request {
    /// Create a new request instance without query parameters.
    #[must_use]
    pub fn new(method: &str, path: &str, body: Option<Value>) -> Self {
        Self {
            method: method.to_string(),
            path: path.to_string(),
            query: HashMap::new(),
            body,
        }
    }

    /// Return the value of the query parameter provided.
    pub fn query_param(&self, name: &str) -> Option<&str> {
        self.query.get(name).map(String::as_str)
    }
}

/// Canned response for the requests matching a method and path.
pub struct Route {
    /// Response body.
    body: String,
    /// HTTP method matched.
    method: String,
    /// Request path matched.
    path: String,
    /// Response status code.
    status: u16,
}

impl Route {
    /// Create a new route instance.
    pub fn new(method: &str, path: &str, status: u16, body: impl Into<String>) -> Self {
        Self {
            body: body.into(),
            method: method.to_string(),
            path: path.to_string(),
            status,
        }
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
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).expect("header to be read");
        if header.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse().expect("content length to be valid");
        }
    }

    // Read body
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body).expect("body to be read");

    // Parse request target
    let url = Url::parse(&format!("http://localhost{target}")).expect("target to be valid");
    Request {
        method,
        path: url.path().to_string(),
        query: url.query_pairs().into_owned().collect(),
        body: serde_json::from_slice(&body).ok(),
    }
}

/// Serve the request available in the stream provided, closing the connection
/// after replying to it.
fn serve(mut stream: TcpStream, routes: &[Route], requests: &Mutex<Vec<Request>>) {
    // Read and record request
    let request = read_request(&mut BufReader::new(&stream));
    let route = routes.iter().find(|r| r.method == request.method && r.path == request.path);
    requests.lock().expect("lock not to be poisoned").push(request);

    // Send response
    let (status, body) = route.map_or((404, ""), |r| (r.status, r.body.as_str()));
    let response = format!(
        "HTTP/1.1 {status} \r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).expect("response to be written");
}
