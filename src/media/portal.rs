//! Screen/window picker via xdg-desktop-portal (the compositor's own dialog).
//! Not on Windows yet.

use anyhow::{Result, anyhow};
#[cfg(target_os = "linux")]
use ashpd::desktop::{
    PersistMode,
    screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType},
};
use tokio::sync::oneshot;

use super::ScreenFd;

pub struct ScreenPick {
    /// None = use the default PipeWire connection (test hook).
    pub fd: Option<ScreenFd>,
    pub node_id: u32,
    pub size: Option<(i32, i32)>,
    /// Dropping this ends the portal session (stops the compositor's capture indicator).
    pub stop: oneshot::Sender<()>,
}

#[cfg(not(target_os = "linux"))]
pub async fn pick_screen() -> Result<ScreenPick> {
    Err(anyhow!("Screen sharing isn't available on Windows yet."))
}

/// Runs on the tokio runtime. Keeps the portal session alive until `stop` fires.
#[cfg(target_os = "linux")]
pub async fn pick_screen() -> Result<ScreenPick> {
    let (res_tx, res_rx) = oneshot::channel();
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let run = async {
            let proxy = Screencast::new().await?;
            let session = proxy.create_session(Default::default()).await?;
            proxy
                .select_sources(
                    &session,
                    SelectSourcesOptions::default()
                        .set_cursor_mode(CursorMode::EMBEDDED)
                        .set_sources(SourceType::MONITOR | SourceType::WINDOW)
                        .set_multiple(false)
                        .set_persist_mode(PersistMode::DoNot),
                )
                .await?;
            let resp = proxy.start(&session, None, Default::default()).await?.response()?;
            let stream = resp.streams().first().cloned().ok_or_else(|| anyhow!("Nothing was selected."))?;
            let fd = proxy.open_pipe_wire_remote(&session, Default::default()).await?;
            anyhow::Ok((proxy, session, stream, fd))
        };
        match run.await {
            Ok((_proxy, session, stream, fd)) => {
                let _ = res_tx.send(Ok((fd, stream.pipe_wire_node_id(), stream.size())));
                let _ = stop_rx.await;
                let _ = session.close().await;
            }
            Err(e) => {
                let _ = res_tx.send(Err(e));
            }
        }
    });
    let (fd, node_id, size) = res_rx.await.map_err(|_| anyhow!("portal task ended"))??;
    Ok(ScreenPick { fd: Some(fd), node_id, size, stop: stop_tx })
}
