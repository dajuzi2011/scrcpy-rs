//! 最小 H.264 SPS 解析：只为了拿到真实分辨率（含裁剪）。
//!
//! 为什么需要它：只有文件回放模式下没有 scrcpy 的 session 包头，
//! 而且 Media Foundation 的输出类型协商有时也需要先知道尺寸。

use crate::error::{Error, Result};

/// 按 Annex-B 起始码切分 NAL 单元，返回 (nal_type, 去掉 1 字节 NAL 头之后的负载)
pub fn annexb_units(data: &[u8]) -> Vec<(u8, &[u8])> {
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut start: Option<usize> = None;

    while i + 2 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if let Some(s) = start {
                let end = trim_trailing_zeros(&data[s..i]);
                if end > 0 {
                    push_unit(&data[s..s + end], &mut out);
                }
            }
            i += 3;
            start = Some(i);
        } else {
            i += 1;
        }
    }
    if let Some(s) = start {
        if s < data.len() {
            let end = trim_trailing_zeros(&data[s..]);
            if end > 0 {
                push_unit(&data[s..s + end], &mut out);
            }
        }
    }
    out
}

fn trim_trailing_zeros(s: &[u8]) -> usize {
    let mut n = s.len();
    while n > 0 && s[n - 1] == 0 {
        n -= 1;
    }
    n
}

fn push_unit<'a>(nal: &'a [u8], out: &mut Vec<(u8, &'a [u8])>) {
    let nal_type = nal[0] & 0x1f;
    out.push((nal_type, &nal[1..]));
}

/// 去掉防竞争字节 00 00 03 -> 00 00
fn remove_emulation_prevention(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for &b in data {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        if b == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
        out.push(b);
    }
    out
}

struct BitReader<'a> {
    data: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit: 0 }
    }

    fn bit(&mut self) -> Result<u32> {
        let byte = self.bit / 8;
        if byte >= self.data.len() {
            return Err(Error::Msg("SPS 解析越界".into()));
        }
        let shift = 7 - (self.bit % 8);
        let v = (self.data[byte] >> shift) & 1;
        self.bit += 1;
        Ok(v as u32)
    }

    fn bits(&mut self, n: u32) -> Result<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Ok(v)
    }

    /// 无符号指数哥伦布
    fn ue(&mut self) -> Result<u32> {
        let mut zeros = 0u32;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return Err(Error::Msg("SPS 指数哥伦布编码非法".into()));
            }
        }
        if zeros == 0 {
            return Ok(0);
        }
        let rest = self.bits(zeros)?;
        Ok((1u32 << zeros) - 1 + rest)
    }

    /// 有符号指数哥伦布
    fn se(&mut self) -> Result<i32> {
        let v = self.ue()? as i64;
        let k = if v % 2 == 0 { -(v / 2) } else { (v + 1) / 2 };
        Ok(k as i32)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dimensions {
    pub width: u32,
    pub height: u32,
}

fn skip_scaling_list(r: &mut BitReader<'_>, size: usize) -> Result<()> {
    let mut last_scale: i32 = 8;
    let mut next_scale: i32 = 8;
    for _ in 0..size {
        if next_scale != 0 {
            let delta = r.se()?;
            next_scale = (last_scale + delta + 256) % 256;
        }
        last_scale = if next_scale == 0 { last_scale } else { next_scale };
    }
    Ok(())
}

/// 从一段 Annex-B 数据里找第一个 SPS 并解析出分辨率
pub fn parse_dimensions(stream: &[u8]) -> Result<Dimensions> {
    for (nal_type, payload) in annexb_units(stream) {
        if nal_type == 7 {
            return parse_sps(payload);
        }
    }
    Err(Error::Msg("流里没有找到 SPS".into()))
}

/// 解析 SPS（含 1 字节 NAL 头之后的内容）
pub fn parse_sps(payload: &[u8]) -> Result<Dimensions> {
    let rbsp = if payload.is_empty() {
        return Err(Error::Msg("SPS 为空".into()));
    } else {
        remove_emulation_prevention(payload)
    };
    let mut r = BitReader::new(&rbsp);

    let profile_idc = r.bits(8)?;
    let _constraint_flags = r.bits(8)?;
    let _level_idc = r.bits(8)?;
    let _sps_id = r.ue()?;

    let mut chroma_format_idc = 1u32; // 4:2:0
    let mut separate_colour_plane = false;

    let high_profile = matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    );
    if high_profile {
        chroma_format_idc = r.ue()?;
        if chroma_format_idc == 3 {
            separate_colour_plane = r.bit()? == 1;
        }
        let _bit_depth_luma = r.ue()?;
        let _bit_depth_chroma = r.ue()?;
        let _qpprime = r.bit()?;
        let seq_scaling_matrix_present = r.bit()? == 1;
        if seq_scaling_matrix_present {
            let count = if chroma_format_idc != 3 { 8 } else { 12 };
            for i in 0..count {
                let present = r.bit()? == 1;
                if present {
                    skip_scaling_list(&mut r, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }

    let _log2_max_frame_num_minus4 = r.ue()?;
    let pic_order_cnt_type = r.ue()?;
    if pic_order_cnt_type == 0 {
        let _log2_max_pic_order_cnt_lsb_minus4 = r.ue()?;
    } else if pic_order_cnt_type == 1 {
        let _delta_pic_order_always_zero = r.bit()?;
        let _offset_for_non_ref_pic = r.se()?;
        let _offset_for_top_to_bottom_field = r.se()?;
        let num_ref_frames_in_cycle = r.ue()?;
        for _ in 0..num_ref_frames_in_cycle {
            let _ = r.se()?;
        }
    }

    let _max_num_ref_frames = r.ue()?;
    let _gaps_in_frame_num_allowed = r.bit()?;

    let pic_width_in_mbs_minus1 = r.ue()?;
    let pic_height_in_map_units_minus1 = r.ue()?;
    let frame_mbs_only = r.bit()? == 1;
    if !frame_mbs_only {
        let _mb_adaptive_frame_field = r.bit()?;
    }
    let _direct_8x8_inference = r.bit()?;

    let frame_cropping = r.bit()? == 1;
    let (mut crop_left, mut crop_right, mut crop_top, mut crop_bottom) = (0u32, 0u32, 0u32, 0u32);
    if frame_cropping {
        crop_left = r.ue()?;
        crop_right = r.ue()?;
        crop_top = r.ue()?;
        crop_bottom = r.ue()?;
    }

    if separate_colour_plane {
        // 4:4:4 独立色彩平面时 chroma_array_type 记为 0
        chroma_format_idc = 0;
    }
    let (sub_width_c, sub_height_c) = match chroma_format_idc {
        1 => (2, 2),
        2 => (2, 1),
        3 => (1, 1),
        _ => (1, 1),
    };
    let crop_unit_x = if chroma_format_idc == 0 { 1 } else { sub_width_c };
    let crop_unit_y = if chroma_format_idc == 0 {
        2 - frame_mbs_only as u32
    } else {
        sub_height_c * (2 - frame_mbs_only as u32)
    };

    let width = (pic_width_in_mbs_minus1 + 1) * 16 - (crop_left + crop_right) * crop_unit_x;
    let height = (2 - frame_mbs_only as u32) * (pic_height_in_map_units_minus1 + 1) * 16
        - (crop_top + crop_bottom) * crop_unit_y;

    if width == 0 || height == 0 {
        return Err(Error::Msg("SPS 解析出的分辨率非法".into()));
    }
    Ok(Dimensions { width, height })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emulation_prevention_roundtrip() {
        let raw = [0u8, 0, 3, 0, 0, 3, 1, 2];
        let out = remove_emulation_prevention(&raw);
        assert_eq!(out, vec![0, 0, 0, 0, 1, 2]);
    }

    #[test]
    fn no_sps_is_error() {
        assert!(parse_dimensions(&[0, 0, 1, 0x65, 0x88]).is_err());
    }
}
