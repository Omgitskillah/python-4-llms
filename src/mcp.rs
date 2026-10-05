use crate::{
    contract::{RunPythonRequest, RunPythonResponse, Status},
    docker::Config,
    supervisor::{Job, launch},
};
use rmcp::{
    Json, RoleServer, ServerHandler,
    handler::server::wrapper::Parameters,
    model::{Implementation, ServerCapabilities, ServerConfig},
    service::RequestContext,
    tool, tool_handler, tool_router,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::{Semaphore, oneshot};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct PythonMcpServer {
    config: Config,
    gate: Arc<Semaphore>,
    cleanup_failed: Arc<AtomicBool>,
    shutdown: CancellationToken,
}

#[tool_router]
impl PythonMcpServer {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            gate: Arc::new(Semaphore::new(1)),
            cleanup_failed: Arc::new(AtomicBool::new(false)),
            shutdown: CancellationToken::new(),
        }
    }

    pub async fn shutdown(&self) {
        self.shutdown.cancel();
        // Wait for the coordinator to verify cleanup before normal process exit.
        let _ = self.gate.acquire().await;
    }

    #[tool(
        name = "run_python",
        description = "Run Python source in a fresh, resource-limited Docker sandbox with Python's standard library available. No host files, secrets, external network, or persistent state. code is 1..=65536 UTF-8 bytes; stdin is text up to 262144 bytes; timeout_ms defaults to 10000 and is capped at 30000. Combined stdout/stderr is capped at 65536 bytes. Check ok and status before using output. Sandbox permissions cannot be changed by this tool."
    )]
    async fn run_python(
        &self,
        Parameters(request): Parameters<RunPythonRequest>,
        context: RequestContext<RoleServer>,
    ) -> Result<Json<RunPythonResponse>, Json<RunPythonResponse>> {
        request.validate().map_err(Json)?;
        let permit = self.gate.clone().try_acquire_owned().map_err(|_| {
            Json(RunPythonResponse::error(
                Status::Busy,
                "Another execution is still running or being cleaned up",
            ))
        })?;
        if self.cleanup_failed.load(Ordering::SeqCst) {
            return Err(Json(RunPythonResponse::error(
                Status::CleanupFailed,
                "An earlier cleanup is unconfirmed; recover containers and restart the server",
            )));
        }
        let stop = context.ct.child_token();
        let _drop_guard = stop.clone().drop_guard();
        let shutdown = self.shutdown.clone();
        let failed = self.cleanup_failed.clone();
        let job = Job {
            config: self.config.clone(),
            request,
        };
        let (sender, receiver) = oneshot::channel();
        // This task owns the permit through cleanup even if rmcp drops the handler.
        tokio::spawn(async move {
            let execution = launch(job, stop.clone());
            tokio::pin!(execution);
            let response = tokio::select! {
                response = &mut execution => response,
                _ = shutdown.cancelled() => { stop.cancel(); execution.await }
            };
            if response.status == Status::CleanupFailed {
                failed.store(true, Ordering::SeqCst);
            }
            drop(permit);
            let _ = sender.send(response);
        });
        let response = receiver.await.unwrap_or_else(|_| {
            self.cleanup_failed.store(true, Ordering::SeqCst);
            RunPythonResponse::error(
                Status::CleanupFailed,
                "Execution coordinator failed; restart after checking containers",
            )
        });
        if response.ok {
            Ok(Json(response))
        } else {
            Err(Json(response))
        }
    }
}

#[tool_handler]
impl ServerHandler for PythonMcpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("python-4-llms", env!("CARGO_PKG_VERSION")))
            .with_instructions("run_python executes independent Python scripts in an isolated Docker container. Print results or JSON to stdout. Use stdin for input text. Package installation is not exposed by the tool; host files are not accessible. Inspect ok and status; partial output after an error is not a successful result.")
    }
}
