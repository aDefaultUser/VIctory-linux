//! `VICTORY_FOG_INPUT=radiance` (off by default): a targeted workaround for
//! the brown veil in rooms, with the fog's composite (container
//! 577248728544d74b, 4812 bytes) reading its quarter-resolution radiance as
//! zeros.
//!
//! The composite reads, in its second table, the quarter-resolution
//! radiance (R9G9B9E5 at (1, 16)), an R10G10B10A2 texture of the same
//! extent at (1, 24) and the depth (R32_FLOAT at (1, 32)), and writes the
//! picture (R16G16B16A16_FLOAT at (1, 48)). `radiance` makes the radiance's
//! entry a null view of its kind, in the table made afresh for that one
//! dispatch. In one room on one machine (a Series S at 1920x1080, a
//! 1280x720 picture inside) that cleared the veil, with the scattered light
//! left on; the veil stayed with the scatter zeroed alone. The fog's density
//! and the scattered light stay; what the radiance adds is gone entirely.
//! Why that input is too bright is not known yet. Only the radiance: what
//! zeros in the texture at (1, 24) would mean is not known (a transmittance
//! of zero is an opaque fog).

use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;

use crate::descriptor::View;

/// What `VICTORY_FOG_INPUT` replaces.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FogInput {
    Off,
    /// The quarter-resolution radiance reads zeros.
    Radiance,
}

fn parse(set: &str) -> Option<FogInput> {
    match set.trim() {
        "" | "0" | "off" => Some(FogInput::Off),
        "radiance" => Some(FogInput::Radiance),
        _ => None,
    }
}

pub(crate) fn mode() -> FogInput {
    static SET: OnceLock<FogInput> = OnceLock::new();
    *SET.get_or_init(|| {
        let set = std::env::var("VICTORY_FOG_INPUT").unwrap_or_default();
        let mode = parse(&set).unwrap_or_else(|| {
            tracing::warn!(set, "VICTORY_FOG_INPUT is off or radiance; off");
            FogInput::Off
        });
        if mode != FogInput::Off {
            tracing::info!(?mode, "an input of the fog's composite reads zeros (VICTORY_FOG_INPUT)");
        }
        mode
    })
}

/// The composite by its container (the first 16 hex digits of its BLAKE3,
/// and its bytes).
const COMPOSITOR: (&str, usize) = ("577248728544d74b", 4812);
pub(crate) const RADIANCE: (u8, u32) = (1, 16);
pub(crate) const MEDIA: (u8, u32) = (1, 24);
pub(crate) const DEPTH: (u8, u32) = (1, 32);
pub(crate) const OUT: (u8, u32) = (1, 48);
/// The extents recorded: the quarter-resolution inputs', the picture's.
const QUARTER: (u64, u32) = (480, 272);
const PICTURE: (u64, u32) = (1280, 720);
const R16G16B16A16_FLOAT: i32 = 10;
const R10G10B10A2_UNORM: i32 = 24;
const R32_FLOAT: i32 = 41;
const R9G9B9E5_SHAREDEXP: u32 = 67;
const TEXTURE2D: u32 = 3;
const SRV_TEXTURE2D: u32 = 4;
const UAV_TEXTURE2D: u32 = 4;

/// Whether a compute container is the composite, with a mode on.
pub(crate) fn is_compositor(hash: &str, bytes: usize) -> bool {
    (hash, bytes) == COMPOSITOR && mode() != FogInput::Off
}

/// Whether the composite's resources are as recorded: one unordered view,
/// the picture's through the table, and the three inputs read there.
pub(crate) fn layout(used: &[victory_shader::UsedResource]) -> bool {
    use victory_shader::{ResourceType, RootType};
    let at = |u: &victory_shader::UsedResource, kind, w| u.kind == kind && u.root == RootType::Table && (u.slot, u.table_offset_dwords) == w;
    let mut uavs = used.iter().filter(|u| u.kind == ResourceType::Uav);
    let one_uav = uavs.next().is_some_and(|u| at(u, ResourceType::Uav, OUT)) && uavs.next().is_none();
    let reads = [RADIANCE, MEDIA, DEPTH].iter().all(|&w| used.iter().any(|u| at(u, ResourceType::Srv, w)));
    one_uav && reads
}

/// Whether the radiance's view (the title's, of memory it placed a texture
/// in), the two other inputs' textures and the picture's texture and view
/// are as recorded.
pub(crate) fn recipe(
    radiance: &View,
    media: &D3D12_RESOURCE_DESC,
    depth: &D3D12_RESOURCE_DESC,
    out: &D3D12_RESOURCE_DESC,
    out_view: Option<&[u32; 10]>,
) -> Result<(), &'static str> {
    // Resource words: dimension, alignment, width, height, then slices and
    // levels, format; view words: format, dimension.
    let View::Placed(false, r, v) = radiance else {
        return Err("the radiance is not a placed texture");
    };
    let width = u64::from(r[4]) | u64::from(r[5]) << 32;
    if r[0] != TEXTURE2D || (width, r[6]) != QUARTER || r[7] & 0xffff != 1 || r[8] != R9G9B9E5_SHAREDEXP {
        return Err("another radiance texture");
    }
    // (A view format of 0: the texture's own.)
    let format = |f: u32, of: u32| f == 0 || crate::resource::pc_format(f) == of;
    if v[1] != SRV_TEXTURE2D || !format(v[0], R9G9B9E5_SHAREDEXP) {
        return Err("another radiance view");
    }
    let flat = |d: &D3D12_RESOURCE_DESC, (w, h): (u64, u32), format: i32| {
        d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE2D && (d.Width, d.Height, d.DepthOrArraySize) == (w, h, 1) && d.Format.0 == format
    };
    if !flat(media, QUARTER, R10G10B10A2_UNORM) {
        return Err("another texture at (1, 24)");
    }
    if !flat(depth, QUARTER, R32_FLOAT) {
        return Err("another depth");
    }
    if !flat(out, PICTURE, R16G16B16A16_FLOAT) {
        return Err("another picture");
    }
    if let Some(v) = out_view
        && (v[1] != UAV_TEXTURE2D || v[2] != 0 || !format(v[0], R16G16B16A16_FLOAT as u32))
    {
        return Err("another view of the picture");
    }
    Ok(())
}

/// A refusal, once for each reason.
pub(crate) fn refuse(why: &'static str, pipeline: &str) {
    static SAID: Mutex<Vec<&str>> = Mutex::new(Vec::new());
    let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
    if !said.contains(&why) {
        said.push(why);
        tracing::warn!(why, pipeline, "fog input: the composite as not recorded; nothing replaced");
    }
}

/// A use, logged at the 1st, 2nd, 4th, 8th... (`slot`: the dynamic slot).
pub(crate) fn used(pipeline: &str, slot: u32) {
    static USES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = USES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    if n.is_power_of_two() {
        tracing::info!(uses = n, slot, mode = ?mode(), pipeline, "fog input: the composite's radiance reads zeros");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings() {
        assert_eq!(parse(""), Some(FogInput::Off));
        assert_eq!(parse("0"), Some(FogInput::Off));
        assert_eq!(parse("off"), Some(FogInput::Off));
        assert_eq!(parse(" radiance "), Some(FogInput::Radiance));
        assert_eq!(parse("Radiance"), None);
        assert_eq!(parse("density"), None);
        assert_eq!(parse("1"), None);
    }

    fn texture((w, h): (u64, u32), format: i32) -> D3D12_RESOURCE_DESC {
        D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: w,
            Height: h,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT(format),
            ..Default::default()
        }
    }

    /// The radiance's placed view as recorded: [`View::Placed`] words.
    fn radiance() -> ([u32; 14], [u32; 16]) {
        let mut r = [0u32; 14];
        r[..9].copy_from_slice(&[TEXTURE2D, 0, 0, 0, 480, 0, 272, 0x1_0001, R9G9B9E5_SHAREDEXP]);
        let mut v = [0u32; 16];
        v[..3].copy_from_slice(&[R9G9B9E5_SHAREDEXP, SRV_TEXTURE2D, 5768]);
        (r, v)
    }

    /// The composite's inputs and picture as recorded, and the ways they are
    /// refused.
    #[test]
    fn recipes() {
        let media = texture(QUARTER, R10G10B10A2_UNORM);
        let depth = texture(QUARTER, R32_FLOAT);
        let out = texture(PICTURE, R16G16B16A16_FLOAT);
        let (r, v) = radiance();
        let placed = View::Placed(false, r, v);
        assert_eq!(recipe(&placed, &media, &depth, &out, None), Ok(()));
        let mut uav = [0u32; 10];
        uav[..2].copy_from_slice(&[R16G16B16A16_FLOAT as u32, UAV_TEXTURE2D]);
        assert_eq!(recipe(&placed, &media, &depth, &out, Some(&uav)), Ok(()));
        let mut level = uav;
        level[2] = 1;
        assert!(recipe(&placed, &media, &depth, &out, Some(&level)).is_err());
        let mut array = uav;
        array[1] = 5;
        assert!(recipe(&placed, &media, &depth, &out, Some(&array)).is_err());

        assert!(recipe(&View::Placed(true, r, v), &media, &depth, &out, None).is_err());
        assert!(recipe(&View::Shader(1, None), &media, &depth, &out, None).is_err());
        for (word, value) in [(0, 4), (4, 960), (6, 270), (7, 0x1_0002), (8, 26)] {
            let mut other = r;
            other[word] = value;
            assert!(recipe(&View::Placed(false, other, v), &media, &depth, &out, None).is_err(), "{word}");
        }
        let mut own = v;
        own[0] = 0;
        assert_eq!(recipe(&View::Placed(false, r, own), &media, &depth, &out, None), Ok(()));
        for (word, value) in [(0, 26), (1, 5)] {
            let mut other = v;
            other[word] = value;
            assert!(recipe(&View::Placed(false, r, other), &media, &depth, &out, None).is_err(), "{word}");
        }

        assert!(recipe(&placed, &texture(QUARTER, R32_FLOAT), &depth, &out, None).is_err());
        assert!(recipe(&placed, &media, &texture((960, 544), R32_FLOAT), &out, None).is_err());
        assert!(recipe(&placed, &media, &depth, &texture((1920, 1080), R16G16B16A16_FLOAT), None).is_err());
        assert!(recipe(&placed, &media, &depth, &texture(PICTURE, 26), None).is_err());
        let mut volume = out;
        volume.Dimension = D3D12_RESOURCE_DIMENSION_TEXTURE3D;
        assert!(recipe(&placed, &media, &depth, &volume, None).is_err());
    }

    /// The composite's resources as recorded, and the ways they are refused.
    #[test]
    fn layouts() {
        use victory_shader::{ResourceType, RootType, UsedResource};
        let used = |kind, root, (slot, table_offset_dwords): (u8, u32)| UsedResource {
            kind,
            root,
            table_offset_dwords,
            lower_bound: 0,
            stride: 0,
            dwords: 8,
            user_data: 0,
            slot,
        };
        let mut recorded: Vec<_> =
            [(0, 304), (0, 624), RADIANCE, MEDIA, DEPTH].iter().map(|&at| used(ResourceType::Srv, RootType::Table, at)).collect();
        recorded.push(used(ResourceType::Cbv, RootType::Table, (1, 56)));
        recorded.push(used(ResourceType::Uav, RootType::Table, OUT));
        assert!(layout(&recorded));
        let mut two = recorded.clone();
        two.push(used(ResourceType::Uav, RootType::Table, (1, 40)));
        assert!(!layout(&two));
        let mut elsewhere = recorded.clone();
        elsewhere.last_mut().unwrap().table_offset_dwords = 40;
        assert!(!layout(&elsewhere));
        let mut no_radiance = recorded.clone();
        no_radiance.remove(2);
        assert!(!layout(&no_radiance));
        let mut by_root = recorded.clone();
        by_root[2].root = RootType::Descriptor;
        assert!(!layout(&by_root));
    }
}
