//! The MTA recording worker owns WGC, D3D, WASAPI and the MP4 sink writer.

use crate::capture::IRect;
use crate::config::Config;
use crate::export::{self, Image};
use crate::recording::{AudioState, Clock, Command, Event};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};
use windows::Graphics::Capture::*;
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::Storage::FileSystem::MoveFileW;
use windows::Win32::System::Com::*;
use windows::Win32::System::Performance::*;
use windows::Win32::System::WinRT::Direct3D11::*;
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};
use windows::Win32::UI::Shell::{FOLDERID_Videos, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
use windows::core::*;

const SECOND: i64 = 10_000_000;
const RATE: i64 = 48_000;
const VIDEO: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

pub struct Completed {
    pub path: PathBuf,
    pub poster: Image,
    pub warning: Option<String>,
}

struct Runtime;

impl Runtime {
    fn new() -> Result<Self> {
        unsafe {
            RoInitialize(RO_INIT_MULTITHREADED)?;
            if let Err(e) = MFStartup(MF_VERSION, MFSTARTUP_FULL) {
                RoUninitialize();
                return Err(e);
            }
        }
        Ok(Self)
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
            RoUninitialize();
        }
    }
}

pub fn run(cfg: Config, region: IRect, audio: AudioState, commands: Receiver<Command>, events: Sender<Event>) {
    let result = Runtime::new().and_then(|_runtime| record(&cfg, region, audio, &commands, &events));
    let _ = events.send(Event::Finished(result.map_err(|e| e.message().to_string())));
}

fn io(e: std::io::Error) -> Error {
    Error::new(E_FAIL, e.to_string())
}

struct Temp(PathBuf);

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub fn directory(cfg: &Config) -> Result<PathBuf> {
    if !cfg.recording_directory.trim().is_empty() {
        return Ok(export::expand(&cfg.recording_directory));
    }
    unsafe {
        let value = SHGetKnownFolderPath(&FOLDERID_Videos, KF_FLAG_DEFAULT, None)?;
        let path = PathBuf::from(value.to_string()?);
        CoTaskMemFree(Some(value.0.cast()));
        Ok(path.join("Screen Recordings"))
    }
}

fn temp_file(dir: &Path, suffix: &str) -> Result<Temp> {
    static ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let name = format!(
        ".glazier-{}-{}-{}-{suffix}.mp4",
        std::process::id(),
        qpc(),
        ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let path = dir.join(name);
    // Reserve without replacing any file, including a file from a previous run.
    std::fs::OpenOptions::new().write(true).create_new(true).open(&path).map_err(io)?;
    Ok(Temp(path))
}

fn publish(temp: &Path, cfg: &Config, dir: &Path) -> Result<PathBuf> {
    let stem = export::file_stem(if cfg.filename_format.is_empty() {
        "recording-%Y-%m-%d_%H-%M-%S"
    } else {
        &cfg.filename_format
    });
    for n in 1..10_000 {
        let path = dir.join(if n == 1 { format!("{stem}.mp4") } else { format!("{stem} ({n}).mp4") });
        match unsafe { MoveFileW(&HSTRING::from(temp.as_os_str()), &HSTRING::from(path.as_os_str())) } {
            Ok(()) => return Ok(path),
            Err(_) if path.exists() => continue,
            Err(e) => return Err(e),
        }
    }
    Err(Error::new(E_FAIL, "Too many recordings with the same name"))
}

fn qpc() -> i64 {
    unsafe {
        let (mut counter, mut frequency) = (0, 0);
        let _ = QueryPerformanceCounter(&mut counter);
        let _ = QueryPerformanceFrequency(&mut frequency);
        (counter as i128 * SECOND as i128 / frequency.max(1) as i128) as i64
    }
}

struct VideoCapture {
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    staging: ID3D11Texture2D,
    crop: D3D11_BOX,
    size: (u32, u32),
    monitor_size: (i32, i32),
}

impl VideoCapture {
    fn new(region: IRect) -> Result<Self> {
        unsafe {
            if !GraphicsCaptureSession::IsSupported()? {
                return Err(Error::new(E_NOTIMPL, "Windows Graphics Capture is unavailable"));
            }
            let mon = MonitorFromRect(
                &RECT { left: region.x, top: region.y, right: region.x + region.w, bottom: region.y + region.h },
                MONITOR_DEFAULTTONEAREST,
            );
            let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
            GetMonitorInfoW(mon, &mut info).ok()?;
            let interop: IGraphicsCaptureItemInterop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
            let item: GraphicsCaptureItem = interop.CreateForMonitor(mon)?;
            let (mut device, mut context) = (None, None);
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
            let device = device.unwrap();
            let context = context.unwrap();
            let dxgi: IDXGIDevice = device.cast()?;
            let direct: IDirect3DDevice = CreateDirect3D11DeviceFromDXGIDevice(&dxgi)?.cast()?;
            let size = item.Size()?;
            let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
                &direct,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                2,
                size,
            )?;
            let session = pool.CreateCaptureSession(&item)?;
            session.SetIsCursorCaptureEnabled(true)?;
            let _ = session.SetIsBorderRequired(false);
            let mut staging = None;
            device.CreateTexture2D(
                &D3D11_TEXTURE2D_DESC {
                    Width: region.w as u32,
                    Height: region.h as u32,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_STAGING,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    ..Default::default()
                },
                None,
                Some(&mut staging),
            )?;
            let x = (region.x - info.rcMonitor.left) as u32;
            let y = (region.y - info.rcMonitor.top) as u32;
            session.StartCapture()?;
            Ok(Self {
                pool,
                session,
                device,
                context,
                staging: staging.unwrap(),
                crop: D3D11_BOX {
                    left: x,
                    top: y,
                    front: 0,
                    right: x + region.w as u32,
                    bottom: y + region.h as u32,
                    back: 1,
                },
                size: (region.w as u32, region.h as u32),
                monitor_size: (size.Width, size.Height),
            })
        }
    }

    fn frame(&self, pixels: &mut [u8]) -> Result<bool> {
        let (w, h) = self.size;
        assert_eq!(pixels.len(), w as usize * h as usize * 4);
        unsafe {
            self.device.GetDeviceRemovedReason()?;
        }
        let mut latest = None;
        // Free-threaded WGC has two buffers. Drain a bounded number to get the
        // freshest frame without ever building an unbounded producer queue.
        for _ in 0..2 {
            match self.pool.TryGetNextFrame() {
                Ok(frame) => {
                    if let Some(old) = latest.replace(frame) {
                        let _ = old.Close();
                    }
                }
                // A null WinRT result becomes Error::empty() in windows-core.
                Err(e) if e.code().is_ok() => break,
                Err(e) => return Err(e),
            }
        }
        let Some(frame) = latest else { return Ok(false) };
        let result = (|| unsafe {
            let size = frame.ContentSize()?;
            if (size.Width, size.Height) != self.monitor_size {
                return Err(Error::new(E_FAIL, "Display layout changed; recording stopped"));
            }
            let surface: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
            let texture: ID3D11Texture2D = surface.GetInterface()?;
            // Only the selected pixels leave GPU memory.
            self.context.CopySubresourceRegion(&self.staging, 0, 0, 0, 0, &texture, 0, Some(&self.crop));
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            for y in 0..h as usize {
                std::ptr::copy_nonoverlapping(
                    mapped.pData.cast::<u8>().add(y * mapped.RowPitch as usize),
                    pixels.as_mut_ptr().add(y * w as usize * 4),
                    w as usize * 4,
                );
            }
            self.context.Unmap(&self.staging, 0);
            self.device.GetDeviceRemovedReason()?;
            Ok(true)
        })();
        let _ = frame.Close();
        result
    }
}

impl Drop for VideoCapture {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

unsafe fn attributes() -> Result<IMFAttributes> {
    unsafe {
        let mut attrs = None;
        MFCreateAttributes(&mut attrs, 4)?;
        Ok(attrs.unwrap())
    }
}

unsafe fn video_type(subtype: &GUID, w: u32, h: u32, fps: u32) -> Result<IMFMediaType> {
    unsafe {
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, ((w as u64) << 32) | h as u64)?;
        t.SetUINT64(&MF_MT_FRAME_RATE, ((fps as u64) << 32) | 1)?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        Ok(t)
    }
}

unsafe fn audio_type(aac: bool) -> Result<IMFMediaType> {
    unsafe {
        let t = MFCreateMediaType()?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        t.SetGUID(&MF_MT_SUBTYPE, if aac { &MFAudioFormat_AAC } else { &MFAudioFormat_PCM })?;
        t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)?;
        t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, RATE as u32)?;
        t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        t.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, if aac { 24_000 } else { RATE as u32 * 4 })?;
        if aac {
            t.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)?;
            t.SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29)?;
        } else {
            t.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 4)?;
            t.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
        }
        Ok(t)
    }
}

struct Encoder {
    writer: IMFSinkWriter,
    video: u32,
    audio: u32,
    description: String,
}

impl Encoder {
    fn new(path: &Path, w: u32, h: u32, fps: u32, hardware: bool) -> Result<Self> {
        unsafe {
            let attrs = attributes()?;
            attrs.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)?;
            attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, hardware as u32)?;
            let writer = MFCreateSinkWriterFromURL(&HSTRING::from(path.as_os_str()), None, &attrs)?;
            let out = video_type(&MFVideoFormat_H264, w, h, fps)?;
            out.SetUINT32(
                &MF_MT_AVG_BITRATE,
                (w as u64 * h as u64 * fps as u64 / 6).clamp(2_000_000, 60_000_000) as u32,
            )?;
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
            let video = writer.AddStream(&out)?;
            let input = video_type(&MFVideoFormat_RGB32, w, h, fps)?;
            input.SetUINT32(&MF_MT_DEFAULT_STRIDE, w * 4)?;
            writer.SetInputMediaType(video, &input, None)?;
            let audio = writer.AddStream(&audio_type(true)?)?;
            writer.SetInputMediaType(audio, &audio_type(false)?, None)?;
            writer.BeginWriting()?;
            let description = encoder_description(&writer, video);
            Ok(Self { writer, video, audio, description })
        }
    }

    fn write(&self, stream: u32, data: &[u8], time: i64, duration: i64) -> Result<()> {
        unsafe {
            let sample = MFCreateSample()?;
            let buffer = MFCreateMemoryBuffer(data.len() as u32)?;
            let mut ptr = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(data.len() as u32)?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(time)?;
            sample.SetSampleDuration(duration.max(1))?;
            self.writer.WriteSample(stream, &sample)
        }
    }
}

fn encoder_description(writer: &IMFSinkWriter, video: u32) -> String {
    unsafe {
        if let Ok(ex) = writer.cast::<IMFSinkWriterEx>() {
            for index in 0..8 {
                let (mut category, mut transform) = (GUID::zeroed(), None);
                if ex.GetTransformForStream(video, index, Some(&mut category), &mut transform).is_err() {
                    break;
                }
                if category == MFT_CATEGORY_VIDEO_ENCODER {
                    if let Some(attrs) = transform.and_then(|t| t.GetAttributes().ok()) {
                        let hardware = attrs.GetStringLength(&MFT_ENUM_HARDWARE_URL_Attribute).is_ok();
                        let mut name = [0u16; 256];
                        let mut count = 0;
                        let _ = attrs.GetString(&MFT_FRIENDLY_NAME_Attribute, &mut name, Some(&mut count));
                        let name = String::from_utf16_lossy(&name[..(count as usize).min(name.len())]);
                        return format!(
                            "{} H.264 ({})",
                            if hardware { "Hardware" } else { "Software" },
                            if name.is_empty() { "Media Foundation" } else { &name }
                        );
                    }
                }
            }
        }
    }
    "Media Foundation H.264 (encoder type unavailable)".into()
}

struct AudioCapture {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    weights: Vec<[f32; 2]>,
    bits: usize,
    float: bool,
    block: usize,
    resampler: Resampler,
    input: Vec<[f32; 2]>,
}

/// Lays packets end to end on the recording timeline. Per-packet timestamps
/// jitter by a few milliseconds, so placing each packet at its own timestamp
/// leaves gaps and overlaps at every boundary, which is audible as crackle.
/// Timestamps only re-anchor the stream after a gap, a pause or clock drift.
struct Resampler {
    /// Input frames per output frame.
    step: f64,
    /// Output frame where the next sample lands, once anchored.
    cursor: Option<i64>,
    /// Read position in input frames relative to the current packet. Values
    /// below zero interpolate from `previous`, the prior packet's last frame.
    phase: f64,
    previous: [f32; 2],
}

impl Resampler {
    // Larger offsets than this are gaps or drift, not timestamp jitter.
    const SLACK: i64 = RATE / 25;

    fn new(rate: u32) -> Self {
        Self { step: rate as f64 / RATE as f64, cursor: None, phase: 0.0, previous: [0.0; 2] }
    }

    fn packet_duration(&self, frames: u32) -> i64 {
        (frames as f64 * SECOND as f64 / (self.step * RATE as f64)).round() as i64
    }

    /// `start` is the packet's first frame on the output timeline.
    fn push(&mut self, input: &[[f32; 2]], start: i64, discontinuity: bool, mix: &mut Mixer) {
        if discontinuity || self.cursor.is_none_or(|c| (c - start).abs() > Self::SLACK) {
            self.cursor = Some(start);
            self.phase = 0.0;
        }
        let mut cursor = self.cursor.unwrap();
        let last = input.len() as f64 - 1.0;
        while self.phase < last {
            let i = self.phase.floor();
            let t = (self.phase - i) as f32;
            let a = if i < 0.0 { self.previous } else { input[i as usize] };
            let b = input[(i + 1.0) as usize];
            mix.add(cursor, [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]);
            cursor += 1;
            self.phase += self.step;
        }
        if let Some(&frame) = input.last() {
            self.phase -= input.len() as f64;
            self.previous = frame;
        }
        self.cursor = Some(cursor);
    }
}

impl AudioCapture {
    fn new(mic: bool) -> Result<Self> {
        unsafe {
            let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_INPROC_SERVER)?;
            let device = enumerator.GetDefaultAudioEndpoint(if mic { eCapture } else { eRender }, eConsole)?;
            let client: IAudioClient = device.Activate(CLSCTX_INPROC_SERVER, None)?;
            let format = client.GetMixFormat()?;
            let f = format.read_unaligned();
            let (subtype, mask) = if f.wFormatTag == 0xfffe {
                let extended = format.cast::<WAVEFORMATEXTENSIBLE>().read_unaligned();
                (extended.SubFormat, extended.dwChannelMask)
            } else {
                (GUID::zeroed(), 0)
            };
            let bits = f.wBitsPerSample;
            let float = f.wFormatTag as u32 == WAVE_FORMAT_IEEE_FLOAT
                || (f.wFormatTag == 0xfffe && subtype == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT);
            let result = client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                if mic { 0 } else { AUDCLNT_STREAMFLAGS_LOOPBACK },
                SECOND,
                0,
                format,
                None,
            );
            CoTaskMemFree(Some(format.cast()));
            result?;
            if f.nChannels == 0
                || f.nSamplesPerSec == 0
                || (f.nBlockAlign as usize) < f.nChannels as usize * bits as usize / 8
                || !(float && bits == 32 || !float && [16, 24, 32].contains(&bits))
            {
                return Err(Error::new(E_NOTIMPL, "Unsupported audio device format"));
            }
            let capture = client.GetService()?;
            client.Start()?;
            Ok(Self {
                client,
                capture,
                weights: channel_weights(f.nChannels as usize, mask),
                bits: f.wBitsPerSample as usize,
                float,
                block: f.nBlockAlign as usize,
                resampler: Resampler::new(f.nSamplesPerSec),
                input: Vec::new(),
            })
        }
    }

    fn drain(&mut self, mix: &mut Mixer, enabled: bool, origin: i64, clock: &Clock, paused: bool) -> Result<()> {
        unsafe {
            // At most one second is buffered by WASAPI. Always release each packet.
            while self.capture.GetNextPacketSize()? > 0 {
                let (mut data, mut frames, mut flags, mut stamp) = (std::ptr::null_mut(), 0, 0, 0);
                self.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut stamp))?;
                if paused {
                    // Resuming re-anchors at the next packet's timestamp.
                    self.resampler.cursor = None;
                } else {
                    let time = if flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 == 0 {
                        stamp as i64 - origin
                    } else {
                        qpc() - origin - self.resampler.packet_duration(frames)
                    };
                    // Muted and silent packets still advance the stream, keeping it continuous.
                    let silent = !enabled || flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null();
                    self.input.resize(frames as usize, [0.0; 2]);
                    if silent {
                        self.input.fill([0.0; 2]);
                    } else {
                        let packet = std::slice::from_raw_parts(data, frames as usize * self.block);
                        for i in 0..frames as usize {
                            self.input[i] = self.frame(packet, i);
                        }
                    }
                    let discontinuity = flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0;
                    self.resampler.push(&self.input, clock.time(time) * RATE / SECOND, discontinuity, mix);
                }
                self.capture.ReleaseBuffer(frames)?;
            }
            Ok(())
        }
    }

    /// One input frame downmixed to stereo.
    fn frame(&self, packet: &[u8], index: usize) -> [f32; 2] {
        let mut out = [0.0; 2];
        for (channel, weights) in self.weights.iter().enumerate() {
            let offset = index * self.block + channel * (self.bits / 8);
            let bytes = &packet[offset..offset + self.bits / 8];
            let value = if self.float {
                f32::from_le_bytes(bytes.try_into().unwrap())
            } else {
                match self.bits {
                    16 => i16::from_le_bytes(bytes.try_into().unwrap()) as f32 / 32768.0,
                    24 => (i32::from_le_bytes([0, bytes[0], bytes[1], bytes[2]]) >> 8) as f32 / 8388608.0,
                    _ => i32::from_le_bytes(bytes.try_into().unwrap()) as f32 / 2147483648.0,
                }
            };
            if value.is_finite() {
                out[0] += value * weights[0];
                out[1] += value * weights[1];
            }
        }
        out
    }
}

fn channel_weights(channels: usize, mask: u32) -> Vec<[f32; 2]> {
    if channels == 1 {
        return vec![[1.0, 1.0]];
    }
    let speakers: Vec<_> = (0..32).filter(|b| mask & (1 << b) != 0).collect();
    let mut weights: Vec<_> = (0..channels)
        .map(|c| {
            let speaker = speakers.get(c).copied().unwrap_or(c as u32);
            match speaker {
                0 => [1.0, 0.0],
                1 => [0.0, 1.0],
                2 | 8 => [0.707, 0.707],
                3 => [0.5, 0.5],
                4 | 6 | 9 => [0.707, 0.0],
                5 | 7 | 10 => [0.0, 0.707],
                _ => [0.5, 0.5],
            }
        })
        .collect();
    let mut gain = [0.0f32; 2];
    for w in &weights {
        for c in 0..2 {
            gain[c] += w[c];
        }
    }
    for w in &mut weights {
        for c in 0..2 {
            w[c] /= gain[c].max(1.0);
        }
    }
    weights
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

struct Mixer {
    samples: Vec<[f32; 2]>,
    next: i64,
}

impl Mixer {
    fn new() -> Self {
        Self { samples: vec![[0.0; 2]; RATE as usize * 2], next: 0 }
    }

    fn add(&mut self, frame: i64, value: [f32; 2]) {
        if frame >= self.next && frame < self.next + self.samples.len() as i64 {
            let i = frame as usize % self.samples.len();
            self.samples[i][0] += value[0];
            self.samples[i][1] += value[1];
        }
    }

    fn write_until(&mut self, end: i64, encoder: &Encoder) -> Result<()> {
        let mut data = [0u8; 480 * 4];
        while self.next < end {
            let count = (end - self.next).min(480) as usize;
            for n in 0..count {
                let frame = self.next + n as i64;
                let i = frame as usize % self.samples.len();
                for (c, v) in self.samples[i].iter().enumerate() {
                    let offset = n * 4 + c * 2;
                    data[offset..offset + 2]
                        .copy_from_slice(&((v.clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes());
                }
                self.samples[i] = [0.0; 2];
            }
            encoder.write(
                encoder.audio,
                &data[..count * 4],
                self.next * SECOND / RATE,
                count as i64 * SECOND / RATE,
            )?;
            self.next += count as i64;
        }
        Ok(())
    }
}

fn ensure_audio(source: &mut Option<AudioCapture>, enabled: bool, mic: bool, events: &Sender<Event>) {
    if enabled && source.is_none() {
        match AudioCapture::new(mic) {
            Ok(s) => *source = Some(s),
            Err(e) => {
                let _ = events.send(Event::Warning(format!(
                    "{} unavailable: {}. Video continues.",
                    if mic { "Microphone" } else { "System audio" },
                    e.message()
                )));
            }
        }
    }
}

fn record(
    cfg: &Config,
    region: IRect,
    mut audio: AudioState,
    commands: &Receiver<Command>,
    events: &Sender<Event>,
) -> Result<Option<Completed>> {
    let capture = VideoCapture::new(region)?;
    let dir = directory(cfg)?;
    std::fs::create_dir_all(&dir).map_err(io)?;
    let temp = temp_file(&dir, "recording")?;
    let fps = cfg.recording_fps.clamp(1, 60);
    let encoder = match Encoder::new(&temp.0, region.w as u32, region.h as u32, fps, true) {
        Ok(e) => e,
        Err(_) => Encoder::new(&temp.0, region.w as u32, region.h as u32, fps, false)?,
    };
    let (mut mic, mut sound) = (None, None);
    ensure_audio(&mut mic, audio.mic, true, events);
    ensure_audio(&mut sound, audio.sound, false, events);
    let mut any_audio = audio.mic || audio.sound;
    let origin = qpc();
    let mut clock = Clock::default();
    let mut paused = false;
    let mut mixer = Mixer::new();
    let mut poster = None;
    // Reuse the CPU readback buffer for every frame, including duplicated frames
    // when the desktop is unchanged.
    let mut image = Image {
        width: region.w as u32,
        height: region.h as u32,
        pixels: vec![0; region.w as usize * region.h as usize * 4],
    };
    let mut have_frame = false;
    let mut last_video = -1;
    let mut warning = None;
    let mut discard = false;
    let _ = events.send(Event::Started(encoder.description.clone()));
    let interval = SECOND / fps as i64;
    let started = Instant::now();
    let result: Result<()> = (|| {
        loop {
            let now = qpc() - origin;
            let mut stop = false;
            for command in commands.try_iter() {
                match command {
                    Command::Stop => stop = true,
                    Command::Discard => {
                        discard = true;
                        stop = true;
                    }
                    Command::Pause(value) => {
                        // Drain before changing the timebase; paused packets never
                        // get assigned timestamps on the resumed timeline.
                        for (s, enabled) in [(&mut mic, audio.mic), (&mut sound, audio.sound)] {
                            if let Some(s) = s {
                                let _ = s.drain(&mut mixer, enabled, origin, &clock, paused);
                            }
                        }
                        paused = value;
                        if paused {
                            clock.pause(now);
                        } else {
                            clock.resume(now);
                        }
                    }
                    Command::Audio(value) => {
                        audio = value;
                        any_audio |= audio.mic || audio.sound;
                        ensure_audio(&mut mic, audio.mic, true, events);
                        ensure_audio(&mut sound, audio.sound, false, events);
                    }
                }
            }
            if stop {
                break;
            }
            for (source, enabled, name) in
                [(&mut mic, audio.mic, "Microphone"), (&mut sound, audio.sound, "System audio")]
            {
                if let Some(s) = source {
                    if let Err(e) = s.drain(&mut mixer, enabled, origin, &clock, paused) {
                        let _ = events.send(Event::Warning(format!(
                            "{name} disconnected: {}. Video continues; toggle it on again to reconnect.",
                            e.message()
                        )));
                        *source = None;
                    }
                }
            }
            let time = clock.time(now);
            let tick = time / interval;
            if !paused && tick > last_video {
                have_frame |= capture.frame(&mut image.pixels)?;
                if !have_frame && started.elapsed().as_secs() >= 5 {
                    return Err(Error::new(E_FAIL, "Screen capture did not deliver a frame"));
                }
                if have_frame {
                    let timestamp = tick * interval;
                    encoder.write(encoder.video, &image.pixels, timestamp, interval)?;
                    last_video = tick;
                    if poster.is_none() {
                        // Keep a small poster, never another full-size recording.
                        let step = (image.width / 480).max(1);
                        let w = image.width / step;
                        let h = image.height / step;
                        let mut pixels = Vec::with_capacity((w * h * 4) as usize);
                        for y in 0..h {
                            for x in 0..w {
                                let i = ((y * step * image.width + x * step) * 4) as usize;
                                pixels.extend_from_slice(&image.pixels[i..i + 3]);
                                pixels.push(255);
                            }
                        }
                        poster = Some(Image { width: w, height: h, pixels });
                    }
                }
                // The holdback lets WASAPI packets arrive before mixing, even
                // when a large frame delays this loop. Later samples are lost.
                mixer.write_until(((time - SECOND / 4).max(0) * RATE / SECOND).max(mixer.next), &encoder)?;
            }
            std::thread::sleep(Duration::from_millis(3));
        }
        if !discard {
            // Keep the last packets still buffered by WASAPI.
            for (s, enabled) in [(&mut mic, audio.mic), (&mut sound, audio.sound)] {
                if let Some(s) = s {
                    let _ = s.drain(&mut mixer, enabled, origin, &clock, paused);
                }
            }
            let end = (clock.time(qpc() - origin) / interval + 1) * interval;
            mixer.write_until(end * RATE / SECOND, &encoder)?;
        }
        Ok(())
    })();
    drop(capture);
    drop(mic);
    drop(sound);
    if discard {
        return Ok(None);
    }
    if let Err(e) = result {
        if poster.is_none() {
            return Err(e);
        }
        warning = Some(format!("Recording stopped early: {}", e.message()));
    }
    unsafe {
        encoder.writer.Finalize()?;
    }
    drop(encoder);
    let poster = poster.ok_or_else(|| Error::new(E_FAIL, "No video frames were captured"))?;
    // Streams cannot be added after BeginWriting. Keep a silent track available
    // for live unmute, then remux it away if audio was never enabled.
    let silent;
    let output = if any_audio {
        &temp.0
    } else {
        silent = temp_file(&dir, "silent")?;
        remove_audio(&temp.0, &silent.0)?;
        &silent.0
    };
    let path = publish(output, cfg, &dir)?;
    Ok(Some(Completed { path, poster, warning }))
}

fn remove_audio(source: &Path, dest: &Path) -> Result<()> {
    unsafe {
        let reader = MFCreateSourceReaderFromURL(&HSTRING::from(source.as_os_str()), None)?;
        reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
        reader.SetStreamSelection(VIDEO, true)?;
        let native = reader.GetNativeMediaType(VIDEO, 0)?;
        let attrs = attributes()?;
        attrs.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)?;
        let writer = MFCreateSinkWriterFromURL(&HSTRING::from(dest.as_os_str()), None, &attrs)?;
        let stream = writer.AddStream(&native)?;
        writer.SetInputMediaType(stream, &native, None)?;
        writer.BeginWriting()?;
        loop {
            let (mut flags, mut sample) = (0, None);
            reader.ReadSample(VIDEO, 0, None, Some(&mut flags), None, Some(&mut sample))?;
            if let Some(s) = sample {
                writer.WriteSample(stream, &s)?;
            }
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                break;
            }
        }
        writer.Finalize()
    }
}

#[cfg(test)]
pub fn probe_microphone() -> Result<f32> {
    let _runtime = Runtime::new()?;
    let mut mic = AudioCapture::new(true)?;
    let origin = qpc();
    std::thread::sleep(Duration::from_millis(200));
    let mut mix = Mixer::new();
    mic.drain(&mut mix, true, origin, &Clock::default(), false)?;
    Ok(mix.samples.iter().flatten().fold(0.0f32, |peak, v| peak.max(v.abs())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jittered_packets_are_continuous_and_gaps_reanchor() {
        for rate in [44_100, 48_000, 96_000, 192_000] {
            let mut mix = Mixer::new();
            let mut resampler = Resampler::new(rate);
            let frames = rate as usize / 100;
            for packet in 0..50i64 {
                // Up to 2 ms of timestamp jitter, as WASAPI reports.
                let jitter = [0, 96, -70, 40, -96][packet as usize % 5];
                resampler.push(&vec![[0.5, -0.25]; frames], packet * 480 + jitter, false, &mut mix);
            }
            // Every frame after the first packet is covered exactly once.
            for frame in 480..23_990 {
                assert_eq!(mix.samples[frame], [0.5, -0.25], "rate {rate}, frame {frame}");
            }
            resampler.push(&vec![[1.0; 2]; frames], 60_000, false, &mut mix);
            assert_eq!(mix.samples[60_000], [1.0; 2]);
            assert_eq!(mix.samples[59_999], [0.0; 2]);
        }
    }

    #[test]
    fn timestamp_fallback_uses_the_device_sample_rate() {
        for rate in [44_100, 48_000, 96_000, 192_000] {
            let resampler = Resampler::new(rate);
            assert_eq!(resampler.packet_duration(rate / 100), SECOND / 100, "rate {rate}");
            assert_eq!(resampler.packet_duration(rate), SECOND, "rate {rate}");
        }
    }

    #[test]
    fn downmix_preserves_stereo_and_includes_surround_center() {
        assert_eq!(channel_weights(2, 3), vec![[1.0, 0.0], [0.0, 1.0]]);
        assert_eq!(channel_weights(1, 4), vec![[1.0, 1.0]]);
        let weights = channel_weights(6, 0x3f);
        assert!(weights[2][0] > 0.0 && weights[2][1] > 0.0);
        for c in 0..2 {
            assert!((weights.iter().map(|w| w[c]).sum::<f32>() - 1.0).abs() < 0.0001);
        }
    }

    #[test]
    fn software_encoder_finalizes_decodable_mp4() {
        let _runtime = Runtime::new().unwrap();
        let temp = temp_file(&std::env::temp_dir(), "software-test").unwrap();
        let encoder = Encoder::new(&temp.0, 320, 180, 30, false).unwrap();
        assert!(encoder.description.starts_with("Software"), "{}", encoder.description);
        let pixels: Vec<u8> = (0..320 * 180).flat_map(|_| [40, 80, 120, 255]).collect();
        let mut mixer = Mixer::new();
        for frame in 0..6 {
            encoder.write(encoder.video, &pixels, frame * SECOND / 30, SECOND / 30).unwrap();
            mixer.write_until((frame + 1) * RATE / 30, &encoder).unwrap();
        }
        unsafe {
            encoder.writer.Finalize().unwrap();
        }
        println!("Software fallback: {}", encoder.description);
        drop(encoder);
        unsafe {
            let reader = MFCreateSourceReaderFromURL(&HSTRING::from(temp.0.as_os_str()), None).unwrap();
            let decoded = MFCreateMediaType().unwrap();
            decoded.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).unwrap();
            decoded.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).unwrap();
            reader.SetCurrentMediaType(VIDEO, None, &decoded).unwrap();
            let (mut flags, mut sample) = (0, None);
            reader.ReadSample(VIDEO, 0, None, Some(&mut flags), None, Some(&mut sample)).unwrap();
            assert!(sample.is_some());
        }
    }
}
