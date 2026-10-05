use python_4_llms::{docker::Config, mcp::PythonMcpServer, supervisor::supervise_stdio};
use rmcp::ServiceExt;
use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, ReadBuf};

struct BoundedLines<R> {
    inner: R,
    length: usize,
}

impl<R: AsyncRead + Unpin> AsyncRead for BoundedLines<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.length as u64 > python_4_llms::contract::MAX_JOB_MESSAGE_BYTES {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "MCP message exceeds byte limit",
            )));
        }
        let previous = buffer.filled().len();
        match Pin::new(&mut this.inner).poll_read(cx, buffer) {
            Poll::Ready(Ok(())) => {
                for byte in &buffer.filled()[previous..] {
                    this.length += 1;
                    if this.length as u64 > python_4_llms::contract::MAX_JOB_MESSAGE_BYTES {
                        // AsyncRead requires an error to leave the caller's
                        // filled region unchanged, even after inner consumed bytes.
                        buffer.set_filled(previous);
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "MCP message exceeds byte limit",
                        )));
                    }
                    if *byte == b'\n' {
                        this.length = 0;
                    }
                }
                Poll::Ready(Ok(()))
            }
            result => result,
        }
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args == ["--help"] || args == ["-h"] {
        eprintln!(
            "Usage: python-mcp [--recover]\n\nStarts a local stdio MCP server exposing run_python.\nRequires PYTHON_MCP_IMAGE (full sha256 image ID) and a running local Docker engine.\nPYTHON_MCP_DOCKER and PYTHON_MCP_DOCKER_SOCKET can select absolute local paths.\n--recover removes expired tool-owned containers and exits."
        );
        std::process::exit(0);
    }
    if !args.is_empty() && args != ["--recover"] && args != ["--supervise-job"] {
        eprintln!("python-mcp: unknown arguments; use --help");
        std::process::exit(1);
    }
    let supervisor = args == ["--supervise-job"];
    let result = if supervisor {
        supervise_stdio().await
    } else {
        serve().await
    };
    if let Err(error) = &result {
        eprintln!("python-mcp: {error}");
    }
    // Dedicated supervisor stdin may have a blocking read pending. A process
    // exit releases it and all pipe descriptors after cleanup has completed.
    std::process::exit(if result.is_ok() { 0 } else { 1 });
}

async fn serve() -> Result<(), String> {
    let config = Config::from_env()?;
    config.preflight().await?;
    let recovered = config.recover().await?;
    if std::env::args().nth(1).as_deref() == Some("--recover") {
        eprintln!("Recovered {recovered} expired sandbox containers");
        return Ok(());
    }
    let server = PythonMcpServer::new(config);
    let input = BoundedLines {
        inner: tokio::io::stdin(),
        length: 0,
    };
    let service = server
        .clone()
        .serve((input, tokio::io::stdout()))
        .await
        .map_err(|e| e.to_string())?;
    let cancellation = service.cancellation_token();
    let waiting = service.waiting();
    tokio::pin!(waiting);
    tokio::select! {
        result = &mut waiting => { result.map_err(|e| e.to_string())?; },
        _ = termination() => { cancellation.cancel(); },
    }
    server.shutdown().await;
    Ok(())
}

async fn termination() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn mcp_frames_reject_oversize_input_without_newline_and_reset_between_frames() {
        let limit = python_4_llms::contract::MAX_JOB_MESSAGE_BYTES as usize;
        let bytes = vec![b'x'; limit + 1];
        let mut input = BoundedLines {
            inner: bytes.as_slice(),
            length: 0,
        };
        assert_eq!(
            input.read_to_end(&mut Vec::new()).await.unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        let mut frames = vec![b'x'; limit - 1];
        frames.push(b'\n');
        frames.extend_from_slice(b"{}\n{}\n");
        let mut input = BoundedLines {
            inner: frames.as_slice(),
            length: 0,
        };
        let mut output = Vec::new();
        input.read_to_end(&mut output).await.unwrap();
        assert_eq!(output, frames);
    }
}
