//! The OTA server of the maintenance start (#3): the reference composition of `rust-mcu-libs`
//! (`examples/firmware/ota-system/src/ota.rs`) on the station network of the product.
//!
//! Upload of the image into the inactive slot (digest of the stream, then of the bytes read back, then the
//! complete ESP validation), publication of OTM1, explicit activation, restart. The transfer, the writer, the
//! validator, OTM1 and the boot are the library's; this file only connects its HTTP requests to them. Token
//! and TLS identity are the reference's: **compiled into the image** (`IOBEWI_OTA_TOKEN`,
//! `IOBEWI_TLS_CERT_PEM`, `IOBEWI_TLS_KEY_PEM`). A SHA-256 proves the integrity of what was received, not who
//! sent it: whoever holds the token can install any image (see `docs/ota.md`).
use alloc::string::String;
use embassy_net::Stack;
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, mutex::Mutex};
use embedded_storage::nor_flash::ReadNorFlash;
use iobewi_esp_firmware_a::platform::SystemBoot;
use iobewi_esp_firmware_storage::shared_flash::ArtifactWriteSession;
use iobewi_esp_flash::{EspFlash, SharedFlash};
use iobewi_esp_tls::EspTlsListener;
use iobewi_firmware_image::esp::{self, Verify};
use iobewi_firmware_slots::Slot;
use iobewi_firmware_update::{
    metadata::{MetadataStore, SessionParams},
    service::{self, BootActivation},
};
use iobewi_http_server::{
    ResponseSent,
    io::Read,
    request::Request,
    response::{IntoResponse, ResponseWriter, StatusCode},
    routing::RequestHandlerService,
};
use sha2::{Digest as _, Sha256};

const TOKEN: &str = match option_env!("IOBEWI_OTA_TOKEN") {
    Some(v) => v,
    None => "",
};
const CERT: &str = match option_env!("IOBEWI_TLS_CERT_PEM") {
    Some(v) => v,
    None => "",
};
const KEY: &str = match option_env!("IOBEWI_TLS_KEY_PEM") {
    Some(v) => v,
    None => "",
};
async fn identity() -> Option<iobewi_esp_tls::mbedtls_rs::SessionConfig<'static>> {
    iobewi_crypto_mbedtls::server_config_from_pem(CERT, KEY).ok()
}
struct State<'a, S> {
    system: Mutex<NoopRawMutex, SystemBoot>,
    flash: &'a SharedFlash,
    store: &'a S,
}
struct Handler<'a, S> {
    state: &'a State<'a, S>,
    activate: bool,
}
struct Activation<'a> {
    system: Mutex<NoopRawMutex, &'a mut SystemBoot>,
    flash: &'a SharedFlash,
}
impl BootActivation for Activation<'_> {
    fn valid_target(&self, target: &str) -> bool {
        matches!(target, "ota_0" | "ota_1")
    }
    async fn activate(&self, target: &str) -> Result<(), ()> {
        let slot = match target {
            "ota_0" => Slot::Zero,
            "ota_1" => Slot::One,
            _ => return Err(()),
        };
        self.system
            .lock()
            .await
            .activate(self.flash, slot)
            .await
            .map_err(|_| ())
    }
}
struct ImageReader<'a>(&'a mut EspFlash);
impl esp::Read for ImageReader<'_> {
    fn read(&mut self, offset: u32, out: &mut [u8]) -> Result<(), ()> {
        self.0.read(offset, out).map_err(|_| ())
    }
}

impl<S: MetadataStore> Handler<'_, S> {
    async fn upload<R: Read>(&self, request: &mut Request<'_, R>, params: SessionParams) -> bool {
        // Keep the sole system owner borrowed through readback and publication.
        let mut system = self.state.system.lock().await;
        let Ok(mut reservation) = system.reserve(self.state.flash).await else {
            return false;
        };
        let permit = reservation.permit();
        let target = if permit.slot().slot == Slot::Zero {
            "ota_0"
        } else {
            "ota_1"
        };
        if !streambewi::ota::valid_upload(
            &params.deployment_id,
            &params.digest,
            params.total as usize,
            permit.region().size,
        ) {
            return false;
        }
        // Invalid digest/conflicting Activating refuse before any erase.
        let Ok(digest) = service::begin(self.state.store, &params).await else {
            return false;
        };
        let Ok(mut session) = ArtifactWriteSession::begin(permit, params.total.into(), digest)
        else {
            return false;
        };
        let partition = session.partition();
        log::info!(
            "ota: writing {} ({} bytes) to {}",
            params.deployment_id,
            params.total,
            target
        );
        let mut buffer = [0u8; 2048];
        let mut remaining = params.total as usize;
        {
            let mut reader = request.body_connection.body().reader();
            while remaining > 0 {
                let limit = remaining.min(buffer.len());
                let Ok(n) = reader.read(&mut buffer[..limit]).await else {
                    return false;
                };
                if n == 0 || !session.append(self.state.flash, &buffer[..n]).await {
                    return false;
                }
                remaining -= n;
            }
        }
        let Ok(committed) = session.finish(self.state.flash).await else {
            return false;
        };
        // Verify actual flash bytes, not only the stream's hash; bound the image
        // validator to the received artifact so stale trailing bytes cannot help.
        {
            let mut flash = self.state.flash.lock().await;
            let mut hash = Sha256::new();
            let mut offset = 0u32;
            while offset < params.total {
                let n = ((params.total - offset) as usize).min(buffer.len());
                if flash
                    .read(partition.offset + offset, &mut buffer[..n])
                    .is_err()
                {
                    return false;
                }
                hash.update(&buffer[..n]);
                offset += n as u32;
            }
            let actual: [u8; 32] = hash.finalize().into();
            if actual != committed.digest.0
                || esp::validate(
                    &mut ImageReader(&mut flash),
                    partition.offset,
                    params.total,
                    &iobewi_esp_boot::MEMORY_MAP,
                    Verify::Full,
                )
                .is_err()
            {
                return false;
            }
        }
        // Physical lock released before ConfigSpace. Reservation still held.
        log::info!(
            "ota: flash digest and image verified; publishing {}",
            params.deployment_id
        );
        service::publish(
            self.state.store,
            params.deployment_id,
            committed,
            String::from(target),
        )
        .await
        .is_ok()
    }
}
impl<S: MetadataStore> RequestHandlerService for Handler<'_, S> {
    async fn call_request_handler_service<R: Read, W: ResponseWriter<Error = R::Error>>(
        &self,
        _state: &(),
        (): (),
        mut request: Request<'_, R>,
        response_writer: W,
    ) -> Result<ResponseSent, W::Error> {
        streambewi::ota::touch(embassy_time::Instant::now().as_secs() as u32);
        let headers = request.parts.headers();
        let authorized = !TOKEN.is_empty()
            && iobewi_http_server::auth::bearer_token(
                headers.get("authorization").and_then(|v| v.as_str().ok()),
            ) == TOKEN;
        let id = headers
            .get("x-deployment-id")
            .and_then(|v| v.as_str().ok())
            .unwrap_or("");
        let digest = headers
            .get("x-image-sha256")
            .and_then(|v| v.as_str().ok())
            .unwrap_or("");
        let len = request.body_connection.content_length();
        let valid_id = streambewi::ota::valid_deployment_id(id);
        let mut reboot = false;
        let (status, message) = if !authorized {
            (StatusCode::UNAUTHORIZED, "unauthorized")
        } else if !valid_id || headers.get("transfer-encoding").is_some() {
            (StatusCode::BAD_REQUEST, "invalid request")
        } else if self.activate {
            if len != 0 {
                (StatusCode::BAD_REQUEST, "activation requires an empty body")
            } else {
                let mut system = self.state.system.lock().await;
                let activation = Activation {
                    system: Mutex::new(&mut system),
                    flash: self.state.flash,
                };
                reboot = service::activate_staged(self.state.store, &activation, id)
                    .await
                    .is_ok();
                if reboot {
                    log::info!("ota: activating {id}; reboot requested");
                    (StatusCode::OK, "activating")
                } else {
                    (StatusCode::CONFLICT, "activation refused")
                }
            }
        } else if len == 0 || len > u32::MAX as usize || digest.len() != 71 {
            (StatusCode::BAD_REQUEST, "length and sha256 digest required")
        } else {
            let params = SessionParams {
                deployment_id: String::from(id),
                digest: String::from(digest),
                total: len as u32,
            };
            if self.upload(&mut request, params).await {
                (StatusCode::OK, "staged")
            } else {
                (
                    StatusCode::CONFLICT,
                    "upload refused or verification failed",
                )
            }
        };
        let connection = match request.body_connection.finalize().await {
            Ok(connection) => connection,
            Err(error) => {
                if reboot {
                    crate::reset_for_rollback().await
                }
                return Err(error);
            }
        };
        let response = (status, message)
            .write_to(connection, response_writer)
            .await;
        // A persisted activation always reboots, including a lost response.
        if reboot {
            crate::reset_for_rollback().await
        }
        response
    }
}

/// True when this image carries what the OTA server needs: a token and a TLS identity.
pub fn configured() -> bool {
    !TOKEN.is_empty() && !CERT.is_empty() && !KEY.is_empty()
}

/// [`serve`] boxed and polled through its vtable, built out of line: its buffers and frames are not those of the
/// product task (the maintenance start has the heap to itself), and the static stack depth of the normal start
/// does not carry them (`tools/stack_depth.py`).
#[inline(never)]
pub fn boxed_serve<'a, S: MetadataStore>(
    stack: Stack<'static>,
    flash: &'a SharedFlash,
    system: SystemBoot,
    store: &'a S,
) -> core::pin::Pin<alloc::boxed::Box<dyn core::future::Future<Output = ()> + 'a>> {
    alloc::boxed::Box::pin(async move {
        serve(stack, flash, system, store).await;
    })
}

/// Serves `/health`, `/ota/upload` and `/ota/activate` over TLS on `stack`, forever (an activation restarts).
pub async fn serve<S: MetadataStore>(
    stack: Stack<'static>,
    flash: &SharedFlash,
    system: SystemBoot,
    store: &S,
) -> ! {
    let tls = iobewi_esp_tls::init(|| None);
    let mut rx = [0u8; 4096];
    let mut tx = [0u8; 4096];
    let mut listener = EspTlsListener::new(stack, tls, identity, &mut rx, &mut tx);
    let state = State {
        system: Mutex::new(system),
        flash,
        store,
    };
    let router = iobewi_http_server::HttpRouter::new()
        .route(
            "/health",
            iobewi_http_server::routing::get(|| async { "ok" }),
        )
        .route(
            "/ota/upload",
            iobewi_http_server::routing::post_service(Handler {
                state: &state,
                activate: false,
            }),
        )
        .route(
            "/ota/activate",
            iobewi_http_server::routing::post_service(Handler {
                state: &state,
                activate: true,
            }),
        );
    log::info!("OTA ready: HTTPS :443; upload then activate by deployment id");
    // picoserve bounds the whole request, body included, by read_request (3 s by default). Measured on the S3
    // by the reference: ~55 KB/s with the erases, so a full 1.5 MiB slot needs ~28 s; budget 120 s. One
    // connection at a time: a slow client holds it that long.
    let mut config = iobewi_http_server::server_config();
    config.timeouts.read_request = embassy_time::Duration::from_secs(120);
    iobewi_http_server::serve_forever_tls_with(&mut listener, &router, &config).await
}
