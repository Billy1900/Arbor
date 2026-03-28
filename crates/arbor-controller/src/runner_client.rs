// runner_client.rs — HTTP client for the runner-agent REST API
// Uses hyper directly to avoid the reqwest dependency chain.
use anyhow::{bail, Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, Uri};
use hyper_util::rt::TokioIo;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::time::timeout;

use arbor_common::proto::*;

pub struct RunnerClient {
    base: String,
}

impl RunnerClient {
    pub fn new(base_url: &str) -> Self {
        Self { base: base_url.trim_end_matches('/').to_string() }
    }

    pub async fn create_vm(&self, req: CreateVmRequest) -> Result<CreateVmResponse> {
        self.post("/vms", &req).await
    }

    pub async fn destroy_vm(&self, vm_id: &str) -> Result<()> {
        self.request_empty(Method::DELETE, &format!("/vms/{}", vm_id)).await
    }

    pub async fn vm_exec(&self, req: VmExecRequest) -> Result<VmExecResponse> {
        self.post("/vms/active/exec", &req).await
    }

    pub async fn checkpoint_vm(
        &self,
        vm_id: &str,
        req: VmCheckpointRequest,
    ) -> Result<VmCheckpointResponse> {
        self.post(&format!("/vms/{}/checkpoint", vm_id), &req).await
    }

    pub async fn restore_vm(&self, req: VmRestoreRequest) -> Result<VmRestoreResponse> {
        self.post("/vms/restore", &req).await
    }

    pub async fn health(&self) -> Result<bool> {
        Ok(self.request_empty(Method::GET, "/health").await.is_ok())
    }

    async fn post<Req, Resp>(&self, path: &str, body: &Req) -> Result<Resp>
    where
        Req: serde::Serialize,
        Resp: serde::de::DeserializeOwned,
    {
        let bytes = self.raw_request(Method::POST, path, Some(serde_json::to_vec(body)?)).await?;
        serde_json::from_slice(&bytes).with_context(|| format!("decode response from {path}"))
    }

    async fn request_empty(&self, method: Method, path: &str) -> Result<()> {
        self.raw_request(method, path, None).await.map(|_| ())
    }

    async fn raw_request(&self, method: Method, path: &str, body: Option<Vec<u8>>) -> Result<Bytes> {
        let addr = self.base.trim_start_matches("http://").trim_start_matches("https://");
        let stream = timeout(Duration::from_secs(5), TcpStream::connect(addr))
            .await.context("connect timeout")?
            .with_context(|| format!("connect to {addr}"))?;

        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        tokio::spawn(conn);

        let body_bytes = body.unwrap_or_default();
        let req = Request::builder()
            .method(method)
            .uri(format!("http://{}{}", addr, path).parse::<Uri>()?)
            .header("content-type", "application/json")
            .header("content-length", body_bytes.len())
            .header("host", addr)
            .body(Full::new(Bytes::from(body_bytes)))?;

        let resp = timeout(Duration::from_secs(60), sender.send_request(req))
            .await.context("request timeout")?.context("send request")?;

        let status = resp.status();
        let bytes = resp.collect().await?.to_bytes();
        if !status.is_success() {
            bail!("runner API {path} returned {status}: {}", String::from_utf8_lossy(&bytes));
        }
        Ok(bytes)
    }
}
