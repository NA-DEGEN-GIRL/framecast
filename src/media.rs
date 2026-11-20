//! Media Foundation: hardware H.264 encoding (sink writer) and decoding (source reader).
//! Both stay on the GPU: frames are D3D11 textures end to end.

use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::core::{GUID, Interface};

use crate::gpu::Gpu;
use crate::util::{Ctx, Res, pcwstr, wide};

pub fn startup() -> Res<()> {
    unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.ctx("MFStartup")
}

fn device_manager(gpu: &Gpu) -> Res<IMFDXGIDeviceManager> {
    let mut token = 0u32;
    let mut mgr = None;
    unsafe {
        MFCreateDXGIDeviceManager(&mut token, &mut mgr).ctx("MFCreateDXGIDeviceManager")?;
        let mgr = mgr.ok_or("null device manager")?;
        mgr.ResetDevice(&gpu.device, token).ctx("ResetDevice")?;
        Ok(mgr)
    }
}

fn attrs(n: u32) -> Res<IMFAttributes> {
    let mut a = None;
    unsafe { MFCreateAttributes(&mut a, n) }.ctx("MFCreateAttributes")?;
    a.ok_or_else(|| "null attributes".into())
}

fn pack(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
}

/// Number of textures the encoder may hold at once. The capture/render side
/// writes into the ring and waits whenever the encoder falls this far behind.
const RING: usize = 6;

pub struct Encoder {
    writer: IMFSinkWriter,
    stream: u32,
    _mgr: IMFDXGIDeviceManager,
    ring: Vec<ID3D11Texture2D>,
    next: usize,
    pub frame_hns: i64,
    submitted: u64,
}

impl Encoder {
    pub fn new(gpu: &Gpu, path: &std::path::Path, cfg: &EncoderConfig) -> Res<Encoder> {
        unsafe {
            let mgr = device_manager(gpu)?;
            let a = attrs(4)?;
            a.SetUnknown(&MF_SINK_WRITER_D3D_MANAGER, &mgr).ctx("d3d manager attr")?;
            a.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1).ctx("hw attr")?;
            a.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4).ctx("container attr")?;
            let w = wide(&path.to_string_lossy());
            let writer = MFCreateSinkWriterFromURL(pcwstr(&w), None, &a).ctx("MFCreateSinkWriterFromURL")?;

            let out = MFCreateMediaType().ctx("media type")?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).ctx("major")?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264).ctx("subtype")?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, cfg.bitrate).ctx("bitrate")?;
            out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).ctx("interlace")?;
            out.SetUINT64(&MF_MT_FRAME_SIZE, pack(cfg.width, cfg.height)).ctx("size")?;
            out.SetUINT64(&MF_MT_FRAME_RATE, pack(cfg.fps, 1)).ctx("rate")?;
            out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1)).ctx("par")?;
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32).ctx("profile")?;
            // Keyframe every second keeps seeking in the editor snappy.
            out.SetUINT32(&MF_MT_MAX_KEYFRAME_SPACING, cfg.fps).ctx("gop")?;
            let stream = writer.AddStream(&out).ctx("AddStream")?;

            let inp = MFCreateMediaType().ctx("media type")?;
            inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).ctx("major")?;
            inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32).ctx("subtype")?;
            inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).ctx("interlace")?;
            inp.SetUINT64(&MF_MT_FRAME_SIZE, pack(cfg.width, cfg.height)).ctx("size")?;
            inp.SetUINT64(&MF_MT_FRAME_RATE, pack(cfg.fps, 1)).ctx("rate")?;
            inp.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1)).ctx("par")?;
            inp.SetUINT32(&MF_MT_DEFAULT_STRIDE, cfg.width * 4).ctx("stride")?;
            writer.SetInputMediaType(stream, &inp, None).ctx("SetInputMediaType")?;
            writer.BeginWriting().ctx("BeginWriting")?;

            let mut ring = Vec::with_capacity(RING);
            for _ in 0..RING {
                ring.push(gpu.texture(
                    cfg.width,
                    cfg.height,
                    DXGI_FORMAT_B8G8R8A8_UNORM,
                    D3D11_BIND_RENDER_TARGET | D3D11_BIND_SHADER_RESOURCE,
                    false,
                )?);
            }
            Ok(Encoder {
                writer,
                stream,
                _mgr: mgr,
                ring,
                next: 0,
                frame_hns: 10_000_000 / cfg.fps as i64,
                submitted: 0,
            })
        }
    }

    fn in_flight(&self) -> u64 {
        let mut st = MF_SINK_WRITER_STATISTICS { cb: std::mem::size_of::<MF_SINK_WRITER_STATISTICS>() as u32, ..Default::default() };
        if unsafe { self.writer.GetStatistics(self.stream, &mut st) }.is_err() {
            return 0;
        }
        st.qwNumSamplesReceived.saturating_sub(st.qwNumSamplesProcessed.max(st.qwNumSamplesEncoded))
    }

    /// Next texture to draw into. Blocks briefly if the encoder still holds it.
    pub fn next_texture(&mut self) -> ID3D11Texture2D {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
        while self.submitted >= RING as u64 - 1
            && self.in_flight() >= RING as u64 - 1
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_micros(500));
        }
        let t = self.ring[self.next].clone();
        self.next = (self.next + 1) % RING;
        t
    }

    pub fn submit(&mut self, tex: &ID3D11Texture2D, time_hns: i64, dur_hns: i64) -> Res<()> {
        unsafe {
            let buf = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, tex, 0, false).ctx("surface buffer")?;
            let len = buf.cast::<IMF2DBuffer>().ctx("IMF2DBuffer")?.GetContiguousLength().ctx("len")?;
            buf.SetCurrentLength(len).ctx("SetCurrentLength")?;
            let s = MFCreateSample().ctx("sample")?;
            s.AddBuffer(&buf).ctx("AddBuffer")?;
            s.SetSampleTime(time_hns).ctx("time")?;
            s.SetSampleDuration(dur_hns).ctx("duration")?;
            self.writer.WriteSample(self.stream, &s).ctx("WriteSample")?;
        }
        self.submitted += 1;
        Ok(())
    }

    pub fn finish(self) -> Res<()> {
        unsafe { self.writer.Finalize() }.ctx("Finalize")
    }
}

pub struct Decoder {
    reader: IMFSourceReader,
    _mgr: IMFDXGIDeviceManager,
    pub width: u32,
    pub height: u32,
    pub duration: f64,
    pub fps: f64,
    /// Our own mip-mapped copy of the current frame (good quality when downscaled).
    pub frame: Option<ID3D11Texture2D>,
    pub frame_srv: Option<ID3D11ShaderResourceView>,
    pub frame_time: i64,
    next_time: i64,
    eof: bool,
}

impl Decoder {
    pub fn open(gpu: &Gpu, path: &std::path::Path) -> Res<Decoder> {
        unsafe {
            let mgr = device_manager(gpu)?;
            let a = attrs(4)?;
            a.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, &mgr).ctx("d3d manager")?;
            a.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1).ctx("hw")?;
            a.SetUINT32(&MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING, 1).ctx("vp")?;
            let w = wide(&path.to_string_lossy());
            let reader = MFCreateSourceReaderFromURL(pcwstr(&w), &a).ctx("open video")?;
            let vs = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
            let _ = reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false);
            reader.SetStreamSelection(vs, true).ctx("select")?;
            let t = MFCreateMediaType().ctx("type")?;
            t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).ctx("major")?;
            t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_ARGB32).ctx("subtype")?;
            reader.SetCurrentMediaType(vs, None, &t).ctx("SetCurrentMediaType")?;
            let cur = reader.GetCurrentMediaType(vs).ctx("current type")?;
            let size = cur.GetUINT64(&MF_MT_FRAME_SIZE).ctx("frame size")?;
            let rate = cur.GetUINT64(&MF_MT_FRAME_RATE).unwrap_or(pack(60, 1));
            let fps = (rate >> 32) as f64 / ((rate & 0xffff_ffff) as f64).max(1.0);
            let dur = reader
                .GetPresentationAttribute(MF_SOURCE_READER_MEDIASOURCE.0 as u32, &MF_PD_DURATION)
                .ok()
                .and_then(|v| u64::try_from(&v).ok())
                .unwrap_or(0);
            Ok(Decoder {
                reader,
                _mgr: mgr,
                width: (size >> 32) as u32,
                height: (size & 0xffff_ffff) as u32,
                duration: dur as f64 / 1e7,
                fps,
                frame: None,
                frame_srv: None,
                frame_time: -1,
                next_time: 0,
                eof: false,
            })
        }
    }

    pub fn seek(&mut self, hns: i64) -> Res<()> {
        let pv = PROPVARIANT::from(hns.max(0));
        unsafe { self.reader.SetCurrentPosition(&GUID::zeroed(), &pv) }.ctx("seek")?;
        self.eof = false;
        self.frame_time = -1;
        self.next_time = 0;
        Ok(())
    }

    /// Decodes one frame into `self.frame`. Returns false at end of stream.
    fn read_one(&mut self, gpu: &Gpu) -> Res<bool> {
        if self.eof {
            return Ok(false);
        }
        unsafe {
            let mut flags = 0u32;
            let mut ts = 0i64;
            let mut sample = None;
            self.reader
                .ReadSample(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, 0, None, Some(&mut flags), Some(&mut ts), Some(&mut sample))
                .ctx("ReadSample")?;
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                self.eof = true;
            }
            let Some(sample) = sample else { return Ok(!self.eof) };
            let dur = sample.GetSampleDuration().unwrap_or(0);
            let buf = sample.GetBufferByIndex(0).ctx("buffer")?;
            let dxgi: IMFDXGIBuffer = buf.cast().ctx("not a GPU buffer")?;
            let mut raw = std::ptr::null_mut();
            dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw).ctx("GetResource")?;
            let src = ID3D11Texture2D::from_raw(raw);
            let sub = dxgi.GetSubresourceIndex().unwrap_or(0);
            let sd = crate::gpu::tex_desc(&src);
            let needs_new = match &self.frame {
                Some(f) => {
                    let d = crate::gpu::tex_desc(f);
                    d.Width != sd.Width || d.Height != sd.Height || d.Format != sd.Format
                }
                None => true,
            };
            if needs_new {
                let t = gpu.texture(sd.Width, sd.Height, sd.Format, D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_RENDER_TARGET, true)?;
                self.frame_srv = Some(gpu.srv(&t)?);
                self.frame = Some(t);
            }
            let dst = self.frame.as_ref().unwrap();
            gpu.ctx.CopySubresourceRegion(dst, 0, 0, 0, 0, &src, sub, None);
            gpu.ctx.GenerateMips(self.frame_srv.as_ref().unwrap());
            self.frame_time = ts;
            self.next_time = ts + dur.max(1);
            Ok(true)
        }
    }

    /// Makes `self.frame` the frame visible at time `t` (100ns). Decodes forward,
    /// seeking only when jumping backwards or far ahead.
    pub fn frame_at(&mut self, gpu: &Gpu, t: i64) -> Res<()> {
        let far = 2 * 10_000_000;
        if self.frame_time >= 0 && (t < self.frame_time || t > self.next_time + far) {
            self.seek(t)?;
        }
        let mut guard = 0;
        while (self.frame_time < 0 || t >= self.next_time) && guard < 600 {
            if !self.read_one(gpu)? {
                break;
            }
            guard += 1;
        }
        Ok(())
    }
}
