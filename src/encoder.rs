//! Media Foundation の SinkWriter で H.264 + AAC の MP4 を書き出す。
//! D3D デバイスマネージャを渡すことで、GPU 上の NV12 テクスチャをそのままハードウェアエンコーダへ流す。
use windows::core::{Interface, Result, GUID, HSTRING, PWSTR};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Media::MediaFoundation::*;

pub const AUDIO_RATE: u32 = 48_000;
pub const AUDIO_CH: u32 = 2;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Codec {
    H264,
    Hevc,
    Av1,
}

impl Codec {
    pub const ALL: [Codec; 3] = [Codec::H264, Codec::Hevc, Codec::Av1];
    pub fn label(self) -> &'static str {
        match self {
            Codec::H264 => "H.264",
            Codec::Hevc => "HEVC",
            Codec::Av1 => "AV1",
        }
    }
    fn subtype(self) -> GUID {
        match self {
            Codec::H264 => MFVideoFormat_H264,
            Codec::Hevc => MFVideoFormat_HEVC,
            Codec::Av1 => MFVideoFormat_AV1,
        }
    }
    /// 同じ画質に必要なビットレートの目安（H.264 を 1 とする）
    pub fn efficiency(self) -> f64 {
        match self {
            Codec::H264 => 1.0,
            Codec::Hevc => 0.7,
            Codec::Av1 => 0.6,
        }
    }
}

/// このPCにハードウェアエンコーダがあるコーデック
pub fn available_codecs() -> Vec<Codec> {
    Codec::ALL
        .into_iter()
        .filter(|c| {
            let out = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: c.subtype() };
            let mut arr: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut n = 0u32;
            unsafe {
                if MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER, None, Some(&out), &mut arr, &mut n).is_err() {
                    return false;
                }
                for i in 0..n as usize {
                    std::ptr::drop_in_place(arr.add(i));
                }
                CoTaskMemFree(Some(arr as _));
            }
            n > 0
        })
        .collect()
}

pub struct Encoder {
    writer: IMFSinkWriter,
    _manager: IMFDXGIDeviceManager,
    video: u32,
    audio: Option<u32>,
    frame_dur: i64,
    /// 実際に使われているエンコーダの名前
    pub name: String,
}

// SinkWriter はフリースレッドなので、録画スレッド間で共有してよい
unsafe impl Send for Encoder {}

fn pack(a: u32, b: u32) -> u64 {
    ((a as u64) << 32) | b as u64
}

impl Encoder {
    pub fn new(path: &str, device: &ID3D11Device, codec: Codec, w: u32, h: u32, fps: u32, bitrate: u32, audio: bool) -> Result<Self> {
        unsafe {
            let mut token = 0u32;
            let mut manager = None;
            MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
            let manager = manager.unwrap();
            manager.ResetDevice(device, token)?;

            let mut attrs = None;
            MFCreateAttributes(&mut attrs, 4)?;
            let attrs = attrs.unwrap();
            attrs.SetUnknown(&MF_SINK_WRITER_D3D_MANAGER, &manager)?;
            attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
            attrs.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)?;
            let writer = MFCreateSinkWriterFromURL(&HSTRING::from(path), None, &attrs)?;

            let out = MFCreateMediaType()?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out.SetGUID(&MF_MT_SUBTYPE, &codec.subtype())?;
            out.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            match codec {
                Codec::H264 => out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?,
                Codec::Hevc => out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH265VProfile_Main_420_8.0 as u32)?,
                Codec::Av1 => {}
            }
            out.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h))?;
            out.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            set_color(&out)?;
            let video = writer.AddStream(&out)?;

            let inp = MFCreateMediaType()?;
            inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            inp.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            inp.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            inp.SetUINT64(&MF_MT_FRAME_SIZE, pack(w, h))?;
            inp.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            inp.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            set_color(&inp)?;
            // エンコーダ設定：上限付き VBR（静かな場面で容量を節約）、キーフレームは 2 秒ごと（シークしやすく）
            let mut params = None;
            MFCreateAttributes(&mut params, 4)?;
            let params = params.unwrap();
            params.SetUINT32(&CODECAPI_AVEncCommonRateControlMode, eAVEncCommonRateControlMode_PeakConstrainedVBR.0 as u32)?;
            params.SetUINT32(&CODECAPI_AVEncCommonMeanBitRate, bitrate)?;
            params.SetUINT32(&CODECAPI_AVEncCommonMaxBitRate, bitrate.saturating_mul(3) / 2)?;
            params.SetUINT32(&CODECAPI_AVEncMPVGOPSize, fps * 2)?;
            writer.SetInputMediaType(video, &inp, &params).or_else(|_| writer.SetInputMediaType(video, &inp, None))?;

            let audio = if audio {
                let out = MFCreateMediaType()?;
                out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
                out.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
                out.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
                out.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, AUDIO_RATE)?;
                out.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, AUDIO_CH)?;
                // AAC エンコーダが受け付けるのは 12000/16000/20000/24000 (byte/s) のみ。24000 = 192kbps
                out.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 24_000)?;
                let idx = writer.AddStream(&out)?;

                let inp = MFCreateMediaType()?;
                inp.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
                inp.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
                inp.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
                inp.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, AUDIO_RATE)?;
                inp.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, AUDIO_CH)?;
                inp.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, AUDIO_CH * 2)?;
                inp.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, AUDIO_RATE * AUDIO_CH * 2)?;
                writer.SetInputMediaType(idx, &inp, None)?;
                Some(idx)
            } else {
                None
            };

            writer.BeginWriting()?;
            let name = encoder_name(&writer, video);
            Ok(Self { writer, _manager: manager, video, audio, frame_dur: 10_000_000 / fps as i64, name })
        }
    }

    /// index 番目のフレームとして書き込む（タイムスタンプは index × 1 フレーム時間の固定レート）
    pub fn write_video(&self, tex: &ID3D11Texture2D, index: u64) -> Result<()> {
        unsafe {
            let buf = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, tex, 0, false)?;
            let len = buf.cast::<IMF2DBuffer>()?.GetContiguousLength()?;
            buf.SetCurrentLength(len)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buf)?;
            sample.SetSampleTime(index as i64 * self.frame_dur)?;
            sample.SetSampleDuration(self.frame_dur)?;
            self.writer.WriteSample(self.video, &sample)
        }
    }

    /// pos（先頭からのサンプルフレーム数）の位置に 16bit ステレオ PCM を書き込む
    pub fn write_audio(&self, pcm: &[i16], pos: u64) -> Result<()> {
        let Some(idx) = self.audio else { return Ok(()) };
        if pcm.is_empty() {
            return Ok(());
        }
        unsafe {
            let bytes = (pcm.len() * 2) as u32;
            let buf = MFCreateMemoryBuffer(bytes)?;
            let mut ptr = std::ptr::null_mut();
            buf.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(pcm.as_ptr() as *const u8, ptr, bytes as usize);
            buf.Unlock()?;
            buf.SetCurrentLength(bytes)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buf)?;
            let frames = (pcm.len() / AUDIO_CH as usize) as i64;
            sample.SetSampleTime(pos as i64 * 10_000_000 / AUDIO_RATE as i64)?;
            sample.SetSampleDuration(frames * 10_000_000 / AUDIO_RATE as i64)?;
            self.writer.WriteSample(idx, &sample)
        }
    }

    pub fn finish(&self) -> Result<()> {
        unsafe { self.writer.Finalize() }
    }
}

/// SinkWriter が選んだエンコーダ MFT の名前（ソフトウェアなら印を付ける）
fn encoder_name(writer: &IMFSinkWriter, stream: u32) -> String {
    (|| -> Result<String> {
        unsafe {
            let mut p = std::ptr::null_mut();
            writer.GetServiceForStream(stream, &GUID::zeroed(), &IMFTransform::IID, &mut p)?;
            let t = IMFTransform::from_raw(p);
            let attrs = t.GetAttributes()?;
            let hw = attrs.GetStringLength(&MFT_ENUM_HARDWARE_URL_Attribute).is_ok();
            let mut s = PWSTR::null();
            let mut n = 0;
            let name = if attrs.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut s, &mut n).is_ok() {
                let v = s.to_string().unwrap_or_default();
                CoTaskMemFree(Some(s.0 as _));
                v
            } else {
                // 名前を属性に持たない MFT（Intel など）は登録情報から引く
                let clsid = attrs.GetGUID(&MFT_TRANSFORM_CLSID_Attribute)?;
                let mut s = PWSTR::null();
                MFTGetInfo(clsid, Some(&mut s), None, None, None, None, None)?;
                let v = s.to_string().unwrap_or_default();
                CoTaskMemFree(Some(s.0 as _));
                v
            };
            Ok(if hw { name } else { format!("{name} (software)") })
        }
    })()
    .unwrap_or_else(|_| "unknown".into())
}

fn set_color(t: &IMFMediaType) -> Result<()> {
    unsafe {
        t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
        t.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
        t.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
        t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
    }
    Ok(())
}
