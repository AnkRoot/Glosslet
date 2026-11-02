//! PRIMARY 与复制数据的原生所有权。面板隐藏不影响已复制内容。

use super::State;
use crate::core::{Failure, MAX_SELECTION_BYTES};
use anyhow::Context;
use std::{
    os::{fd::AsFd, unix::net::UnixStream},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::AsyncReadExt;
use wayland_client::{Proxy, QueueHandle, globals::GlobalList, protocol::wl_seat};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};

pub(super) type Manager = ExtDataControlManagerV1;
pub(super) type Device = ExtDataControlDeviceV1;
pub(super) type Offer = ExtDataControlOfferV1;
pub(super) type Source = ExtDataControlSourceV1;
const TEXT_MIMES: [&str; 3] = ["text/plain;charset=utf-8", "text/plain", "UTF8_STRING"];
const CAPTURE_TIMEOUT: Duration = Duration::from_millis(800);

#[derive(Default)]
pub(super) struct OfferData(Mutex<Vec<String>>);
pub(super) struct SourceData(Arc<str>);

enum Primary {
    Unavailable,
    Empty,
    Offered(Offer),
}
pub(super) enum Event {
    PrimaryChanged,
    Finished,
    None,
}

pub(super) struct Selection {
    manager: Manager,
    device: Option<Device>,
    primary: Primary,
    clipboard_offer: Option<Offer>,
    clipboard: Option<Source>,
}

impl Selection {
    pub(super) fn bind(globals: &GlobalList, qh: &QueueHandle<State>) -> anyhow::Result<Self> {
        let manager = globals
            .bind(qh, 1..=1, ())
            .context("桌面未提供 ext-data-control-v1")?;
        Ok(Self {
            manager,
            device: None,
            primary: Primary::Unavailable,
            clipboard_offer: None,
            clipboard: None,
        })
    }

    pub(super) fn attach_seat(&mut self, seat: &wl_seat::WlSeat, qh: &QueueHandle<State>) {
        self.device = Some(self.manager.get_data_device(seat, qh, ()));
    }

    pub(super) fn read(&self) -> Result<Option<UnixStream>, Failure> {
        let offer = match &self.primary {
            Primary::Empty => return Ok(None),
            Primary::Unavailable => return Err(Failure::CaptureUnavailable),
            Primary::Offered(offer) => offer,
        };
        let data = offer
            .data::<OfferData>()
            .ok_or(Failure::CaptureUnavailable)?;
        let mimes = data.0.lock().map_err(|_| Failure::CaptureUnavailable)?;
        let mime = TEXT_MIMES
            .iter()
            .find(|mime| mimes.iter().any(|value| value == **mime))
            .ok_or(Failure::CaptureUnavailable)?;
        let (read, write) = UnixStream::pair().map_err(|_| Failure::CaptureUnavailable)?;
        offer.receive((*mime).into(), write.as_fd());
        Ok(Some(read))
    }

    pub(super) fn copy(&mut self, text: String, qh: &QueueHandle<State>) -> bool {
        let Some(device) = &self.device else {
            return false;
        };
        let source = self
            .manager
            .create_data_source(qh, SourceData(Arc::from(text)));
        for mime in TEXT_MIMES {
            source.offer(mime.into());
        }
        device.set_selection(Some(&source));
        self.clipboard = Some(source);
        true
    }

    pub(super) fn device_event(
        &mut self,
        device: &Device,
        event: ext_data_control_device_v1::Event,
    ) -> Event {
        if self.device.as_ref() != Some(device) {
            return Event::None;
        }
        match event {
            ext_data_control_device_v1::Event::PrimarySelection { id } => {
                let primary = id.map_or(Primary::Empty, Primary::Offered);
                if let Primary::Offered(previous) = std::mem::replace(&mut self.primary, primary) {
                    previous.destroy();
                }
                Event::PrimaryChanged
            }
            ext_data_control_device_v1::Event::Selection { id } => {
                if let Some(previous) = std::mem::replace(&mut self.clipboard_offer, id) {
                    previous.destroy();
                }
                Event::None
            }
            ext_data_control_device_v1::Event::Finished => Event::Finished,
            _ => Event::None,
        }
    }

    pub(super) fn source_event(
        &mut self,
        source: &Source,
        event: ext_data_control_source_v1::Event,
        data: &SourceData,
    ) -> Option<(std::os::fd::OwnedFd, Arc<str>)> {
        match event {
            ext_data_control_source_v1::Event::Send { mime_type, fd }
                if TEXT_MIMES.contains(&mime_type.as_str()) =>
            {
                Some((fd, data.0.clone()))
            }
            ext_data_control_source_v1::Event::Cancelled => {
                if self.clipboard.as_ref() == Some(source) {
                    self.clipboard = None;
                }
                source.destroy();
                None
            }
            _ => None,
        }
    }

    pub(super) fn detach_seat(&mut self) {
        if let Some(device) = self.device.take() {
            device.destroy();
        }
        if let Primary::Offered(offer) = std::mem::replace(&mut self.primary, Primary::Unavailable)
        {
            offer.destroy();
        }
        if let Some(offer) = self.clipboard_offer.take() {
            offer.destroy();
        }
        if let Some(source) = self.clipboard.take() {
            source.destroy();
        }
    }
}

impl OfferData {
    pub(super) fn event(&self, event: ext_data_control_offer_v1::Event) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event
            && let Ok(mut mimes) = self.0.lock()
            && TEXT_MIMES.contains(&mime_type.as_str())
            && !mimes.contains(&mime_type)
        {
            mimes.push(mime_type);
        }
    }
}

pub(super) async fn read_selection(read: UnixStream) -> std::result::Result<String, Failure> {
    read.set_nonblocking(true)
        .map_err(|_| Failure::CaptureUnavailable)?;
    let stream = tokio::net::UnixStream::from_std(read).map_err(|_| Failure::CaptureUnavailable)?;
    let mut bytes = Vec::new();
    tokio::time::timeout(
        CAPTURE_TIMEOUT,
        stream
            .take(MAX_SELECTION_BYTES as u64 + 1)
            .read_to_end(&mut bytes),
    )
    .await
    .map_err(|_| Failure::CaptureTimeout)?
    .map_err(|_| Failure::CaptureUnavailable)?;
    if bytes.len() > MAX_SELECTION_BYTES {
        return Err(Failure::SelectionTooLarge);
    }
    let text = String::from_utf8(bytes).map_err(|_| Failure::InvalidText)?;
    Ok(text.trim_matches('\0').trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn selection_transfer_validates_size_utf8_and_deadline() {
        for (bytes, expected) in [
            (b" word ".to_vec(), Ok("word".into())),
            (
                vec![b'a'; MAX_SELECTION_BYTES + 1],
                Err(Failure::SelectionTooLarge),
            ),
            (vec![0xff], Err(Failure::InvalidText)),
        ] {
            let (read, write) = UnixStream::pair().unwrap();
            write.set_nonblocking(true).unwrap();
            let mut write = tokio::net::UnixStream::from_std(write).unwrap();
            let sender = tokio::spawn(async move {
                let _ = write.write_all(&bytes).await;
            });
            assert_eq!(read_selection(read).await, expected);
            sender.await.unwrap();
        }
        let (read, _write) = UnixStream::pair().unwrap();
        assert_eq!(read_selection(read).await, Err(Failure::CaptureTimeout));
    }

    #[tokio::test]
    async fn cancelling_selection_closes_the_transfer_endpoint() {
        let (read, write) = UnixStream::pair().unwrap();
        write.set_nonblocking(true).unwrap();
        let mut write = tokio::net::UnixStream::from_std(write).unwrap();
        let task = tokio::spawn(read_selection(read));
        tokio::task::yield_now().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(write.write_all(b"not retained").await.is_err());
    }
}
