//! Browser uploads into the media library, with progress (WASM only).
//!
//! Each picked file goes out in its own `XMLHttpRequest` with the `File`
//! itself in a `FormData`, so the browser streams it from disk instead of the
//! page reading it into memory first. `xhr.upload` reports how much has been
//! sent; the updates reach the app as `AppMessage::MediaUpload` and are shown
//! as rows next to the URL downloads.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use egui::{Context, Ui};
use strom_types::media_download::MediaDownloadState;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{Event, File, FormData, HtmlInputElement, ProgressEvent, XmlHttpRequest};

use crate::media::{transfer_row, upload_outcome, RowAction, TransferRow};
use crate::state::AppMessage;

/// Least time between two progress messages for one upload, so a fast upload
/// sends at most about four per second. The last one is always sent.
const PROGRESS_INTERVAL_MS: f64 = 250.0;

/// Local upload ids. They live only in this page and never mix with the
/// server's download job ids, which are kept in a separate list.
static NEXT_UPLOAD_ID: AtomicU64 = AtomicU64::new(1);

/// What happened to an upload, sent from the XHR callbacks to the app.
#[derive(Debug, Clone)]
pub struct UploadUpdate {
    pub id: u64,
    pub event: UploadEvent,
}

#[derive(Debug, Clone)]
pub enum UploadEvent {
    /// The request was created; `size` is the file's size.
    Started {
        filename: String,
        directory: String,
        size: u64,
    },
    /// Bytes of the request body sent so far.
    Progress { sent: u64, total: u64 },
    /// The server stored the file.
    Done,
    /// The upload failed, with the reason.
    Failed(String),
    /// The operator cancelled the upload.
    Cancelled,
}

/// One upload as shown in the Media panel.
struct Upload {
    id: u64,
    filename: String,
    directory: String,
    size: u64,
    sent: u64,
    total: u64,
    state: MediaDownloadState,
    error: Option<String>,
}

/// A running request and the callbacks it calls.
///
/// The JS functions the XHR holds point into these `Closure`s, so they must
/// live as long as the request can still fire. The `Transfer` stays in
/// `Uploads::transfers` until the final message (done, failed, cancelled) is
/// applied in an egui frame, then it is dropped: that runs outside any XHR
/// callback, after the request has ended, so nothing calls a freed closure.
/// Drop also detaches the handlers, so a stray late event finds none.
///
/// The callbacks capture only the message sender, the egui context and the
/// upload id. They get the XHR from the event target instead of capturing it,
/// and nothing they hold points back at `transfers`, so there is no cycle.
struct Transfer {
    xhr: XmlHttpRequest,
    _callbacks: Vec<Closure<dyn FnMut(Event)>>,
}

impl Drop for Transfer {
    fn drop(&mut self) {
        if let Ok(upload) = self.xhr.upload() {
            upload.set_onprogress(None);
        }
        self.xhr.set_onload(None);
        self.xhr.set_onerror(None);
        self.xhr.set_onabort(None);
        self.xhr.set_ontimeout(None);
        // Only reached with a request still in flight when the page itself
        // goes away; stop it rather than leave it running without a row.
        if self.xhr.ready_state() != XmlHttpRequest::DONE {
            let _ = self.xhr.abort();
        }
    }
}

/// Uploads started from this page: their rows, and the requests still running.
pub struct Uploads {
    rows: Vec<Upload>,
    transfers: Rc<RefCell<HashMap<u64, Transfer>>>,
}

impl Uploads {
    pub fn new() -> Self {
        Self {
            rows: Vec::new(),
            transfers: Rc::new(RefCell::new(HashMap::new())),
        }
    }

    /// Open the browser's file picker and upload every picked file into
    /// `directory`, each in its own request, all at once.
    pub fn pick_files(
        &self,
        api: &crate::api::ApiClient,
        ctx: &Context,
        tx: &std::sync::mpsc::Sender<AppMessage>,
        directory: &str,
    ) {
        let Some(document) = web_sys::window().and_then(|w| w.document()) else {
            return;
        };
        let Ok(input) = document
            .create_element("input")
            .map(|e| e.unchecked_into::<HtmlInputElement>())
        else {
            return;
        };
        input.set_type("file");
        input.set_multiple(true);
        let _ = input.style().set_property("display", "none");
        if let Some(body) = document.body() {
            let _ = body.append_child(&input);
        }

        let url = format!(
            "{}/media/upload?path={}",
            api.base_url(),
            urlencoding::encode(directory)
        );
        let authorization = api.authorization();
        let directory = directory.to_string();
        let transfers = self.transfers.clone();
        let ctx = ctx.clone();
        let tx = tx.clone();

        // One function for both "change" (files picked) and "cancel" (picker
        // closed): exactly one of them fires, and `once_into_js` frees the
        // closure after that call. The function holds `transfers` only until
        // then, and `transfers` never holds it, so this is no cycle.
        let on_pick = Closure::once_into_js(move |event: Event| {
            let Some(input) = event
                .target()
                .and_then(|t| t.dyn_into::<HtmlInputElement>().ok())
            else {
                return;
            };
            if let Some(files) = input.files() {
                for i in 0..files.length() {
                    if let Some(file) = files.get(i) {
                        start(
                            &file,
                            &url,
                            authorization.as_deref(),
                            &directory,
                            &ctx,
                            &tx,
                            &transfers,
                        );
                    }
                }
            }
            input.remove();
        });
        input.set_onchange(Some(on_pick.unchecked_ref()));
        let _ = input.add_event_listener_with_callback("cancel", on_pick.unchecked_ref());
        input.click();
    }

    /// Apply an update. Returns true when an upload just finished into
    /// `current_path`, so the listing should be refreshed.
    pub fn apply(&mut self, update: UploadUpdate, current_path: &str) -> bool {
        let id = update.id;
        if let UploadEvent::Started {
            filename,
            directory,
            size,
        } = update.event
        {
            self.rows.push(Upload {
                id,
                filename,
                directory,
                size,
                sent: 0,
                total: size,
                state: MediaDownloadState::Downloading,
                error: None,
            });
            return false;
        }

        let finished = !matches!(update.event, UploadEvent::Progress { .. });
        if finished {
            // The request has ended: let go of it and its callbacks (see
            // `Transfer`). Taken out first so the borrow ends before drop.
            let transfer = self.transfers.borrow_mut().remove(&id);
            drop(transfer);
        }

        let Some(row) = self.rows.iter_mut().find(|r| r.id == id) else {
            return false;
        };
        if row.state.is_finished() {
            return false;
        }
        match update.event {
            UploadEvent::Started { .. } => false,
            UploadEvent::Progress { sent, total } => {
                row.sent = sent;
                if total > 0 {
                    row.total = total;
                }
                false
            }
            UploadEvent::Done => {
                row.state = MediaDownloadState::Done;
                row.sent = row.total;
                row.directory == current_path
            }
            UploadEvent::Failed(error) => {
                row.state = MediaDownloadState::Failed;
                row.error = Some(error);
                false
            }
            UploadEvent::Cancelled => {
                row.state = MediaDownloadState::Cancelled;
                false
            }
        }
    }

    /// One row per upload, below the URL downloads.
    pub fn render_rows(&mut self, ui: &mut Ui) {
        let mut cancel = None;
        let mut dismiss = None;
        for row in &self.rows {
            let path = if row.directory.is_empty() {
                row.filename.clone()
            } else {
                format!("{}/{}", row.directory, row.filename)
            };
            let hover = format!("Upload into media/{}", row.directory);
            let bytes = if row.state == MediaDownloadState::Done {
                row.size
            } else {
                row.sent
            };
            let action = transfer_row(
                ui,
                TransferRow {
                    name: &row.filename,
                    done_label: &path,
                    bytes,
                    total: Some(row.total),
                    state: row.state,
                    error: row.error.as_deref(),
                    hover: &hover,
                    cancel_hint: "Cancel upload",
                },
            );
            match action {
                RowAction::Cancel => cancel = Some(row.id),
                RowAction::Dismiss => dismiss = Some(row.id),
                RowAction::None => {}
            }
        }
        if let Some(id) = cancel {
            // Fires `abort` right away, which reports the upload cancelled.
            if let Some(transfer) = self.transfers.borrow().get(&id) {
                let _ = transfer.xhr.abort();
            }
        }
        if let Some(id) = dismiss {
            self.rows.retain(|r| r.id != id);
        }
    }
}

impl Default for Uploads {
    fn default() -> Self {
        Self::new()
    }
}

/// Start uploading one file and register its request.
fn start(
    file: &File,
    url: &str,
    authorization: Option<&str>,
    directory: &str,
    ctx: &Context,
    tx: &std::sync::mpsc::Sender<AppMessage>,
    transfers: &Rc<RefCell<HashMap<u64, Transfer>>>,
) {
    let id = NEXT_UPLOAD_ID.fetch_add(1, Ordering::Relaxed);
    let notify = {
        let ctx = ctx.clone();
        let tx = tx.clone();
        move |event: UploadEvent| {
            let _ = tx.send(AppMessage::MediaUpload(UploadUpdate { id, event }));
            ctx.request_repaint();
        }
    };

    notify(UploadEvent::Started {
        filename: file.name(),
        directory: directory.to_string(),
        size: file.size() as u64,
    });
    match send(file, url, authorization, notify.clone()) {
        Ok(transfer) => {
            transfers.borrow_mut().insert(id, transfer);
        }
        Err(e) => {
            tracing::error!("Could not start upload of {}: {:?}", file.name(), e);
            notify(UploadEvent::Failed(format!(
                "Could not start the upload: {:?}",
                e
            )));
        }
    }
}

/// Build the request with its callbacks and send it.
fn send(
    file: &File,
    url: &str,
    authorization: Option<&str>,
    notify: impl Fn(UploadEvent) + Clone + 'static,
) -> Result<Transfer, JsValue> {
    let xhr = XmlHttpRequest::new()?;
    xhr.open_with_async("POST", url, true)?;
    // Same header the ApiClient adds. The login cookie is sent by the browser
    // on its own: the API is same-origin, where XHR sends cookies without
    // `withCredentials`, exactly like the fetch requests reqwest makes.
    if let Some(value) = authorization {
        xhr.set_request_header("Authorization", value)?;
    }
    let form = FormData::new()?;
    form.append_with_blob_and_filename("file", file, &file.name())?;

    let on_progress = {
        let notify = notify.clone();
        let mut last_sent_at = f64::NEG_INFINITY;
        Closure::<dyn FnMut(Event)>::new(move |event: Event| {
            let Some(progress) = event.dyn_ref::<ProgressEvent>() else {
                return;
            };
            let sent = progress.loaded() as u64;
            let total = if progress.length_computable() {
                progress.total() as u64
            } else {
                0
            };
            let now = js_sys::Date::now();
            // Always pass the final event on; throttle the rest, including
            // when the browser cannot tell the total.
            let complete = total > 0 && sent >= total;
            if !complete && now - last_sent_at < PROGRESS_INTERVAL_MS {
                return;
            }
            last_sent_at = now;
            notify(UploadEvent::Progress { sent, total });
        })
    };
    let on_load = {
        let notify = notify.clone();
        Closure::<dyn FnMut(Event)>::new(move |event: Event| {
            let Some(xhr) = event
                .target()
                .and_then(|t| t.dyn_into::<XmlHttpRequest>().ok())
            else {
                notify(UploadEvent::Failed("No response from the server".into()));
                return;
            };
            let status = xhr.status().unwrap_or(0);
            let body = xhr.response_text().ok().flatten().unwrap_or_default();
            notify(match upload_outcome(status, &body) {
                Ok(message) => {
                    tracing::info!("Upload finished: {}", message);
                    UploadEvent::Done
                }
                Err(error) => {
                    tracing::error!("Upload failed: {}", error);
                    UploadEvent::Failed(error)
                }
            });
        })
    };
    let on_error = {
        let notify = notify.clone();
        Closure::<dyn FnMut(Event)>::new(move |_: Event| {
            notify(UploadEvent::Failed(
                "Network error: the upload did not reach the server".into(),
            ));
        })
    };
    let on_abort = {
        let notify = notify.clone();
        Closure::<dyn FnMut(Event)>::new(move |_: Event| notify(UploadEvent::Cancelled))
    };
    let on_timeout = Closure::<dyn FnMut(Event)>::new(move |_: Event| {
        notify(UploadEvent::Failed("The upload timed out".into()));
    });

    xhr.upload()?
        .set_onprogress(Some(on_progress.as_ref().unchecked_ref()));
    xhr.set_onload(Some(on_load.as_ref().unchecked_ref()));
    xhr.set_onerror(Some(on_error.as_ref().unchecked_ref()));
    xhr.set_onabort(Some(on_abort.as_ref().unchecked_ref()));
    xhr.set_ontimeout(Some(on_timeout.as_ref().unchecked_ref()));

    let transfer = Transfer {
        xhr,
        _callbacks: vec![on_progress, on_load, on_error, on_abort, on_timeout],
    };
    // On failure the transfer is dropped here, which detaches the handlers.
    transfer.xhr.send_with_opt_form_data(Some(&form))?;
    Ok(transfer)
}
