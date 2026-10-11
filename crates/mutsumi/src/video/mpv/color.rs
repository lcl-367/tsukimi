use gtk::gdk;
use once_cell::sync::Lazy;
use tokio::sync::watch;

/// Wayland image-description values, kept independent of GTK for the proxy thread.
/// Named values use the color-management protocol's enums, not CICP code points.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct ColorDescription {
    pub primaries_named: Option<u32>,
    pub primaries: Option<[i32; 8]>,
    pub tf_named: Option<u32>,
    pub tf_power: Option<u32>,
    pub luminances: Option<(u32, u32, u32)>,
    pub target_primaries: Option<[i32; 8]>,
    pub target_luminance: Option<(u32, u32)>,
    pub max_cll: Option<u32>,
    pub max_fall: Option<u32>,
}

impl ColorDescription {
    /// The protocol's ordinary sRGB encoding, including its default luminances.
    pub(crate) fn srgb() -> Self {
        let primaries = [
            640000, 330000, 300000, 600000, 150000, 60000, 312700, 329000,
        ];
        Self {
            primaries_named: Some(1),
            primaries: Some(primaries),
            tf_named: Some(9),
            luminances: Some((2000, 80, 80)),
            target_primaries: Some(primaries),
            target_luminance: Some((2000, 80)),
            ..Self::default()
        }
    }

    /// Validate the encodings our GDK texture bridge can represent.
    /// This part is pure Rust and may also be called by the proxy thread.
    pub(crate) fn cicp(&self) -> Result<(u32, u32), String> {
        let primaries = match self.primaries_named {
            Some(1) => 1,  // sRGB / BT.709
            Some(3) => 5,  // PAL / BT.601 625
            Some(4) => 6,  // NTSC / BT.601 525
            Some(6) => 9,  // BT.2020
            Some(9) => 12, // Display P3
            value => return Err(format!("unsupported Wayland color primaries: {value:?}")),
        };
        let transfer = match self.tf_named {
            Some(2) => 4,   // gamma 2.2
            Some(3) => 5,   // gamma 2.8
            Some(5) => 8,   // linear
            Some(9) => 13,  // sRGB
            Some(11) => 16, // PQ
            Some(13) => 18, // HLG
            value => return Err(format!("unsupported Wayland transfer function: {value:?}")),
        };
        Ok((primaries, transfer))
    }

    /// Must run on the GTK thread. These are rendered RGB pixels, not source YUV.
    pub(crate) fn to_gdk_color_state(&self) -> Result<gdk::ColorState, String> {
        let (primaries, transfer) = self.cicp()?;
        let params = gdk::CicpParams::new();
        params.set_color_primaries(primaries);
        params.set_transfer_function(transfer);
        params.set_matrix_coefficients(0);
        params.set_range(gdk::CicpRange::Full);
        params
            .build_color_state()
            .map_err(|error| error.to_string())
    }

    pub(crate) fn to_gdk_dmabuf_color_state(
        &self, fourcc: u32,
    ) -> Result<Option<gdk::ColorState>, String> {
        // GTK has no public fourcc classification API. These are the RGB
        // formats supported by GTK 4.24.1. YUV needs matrix/range information
        // that this bridge does not carry, so leave its GTK defaults intact.
        if matches!(
            &fourcc.to_le_bytes(),
            b"AR24"
                | b"BA24"
                | b"AB24"
                | b"RA24"
                | b"XR24"
                | b"BX24"
                | b"XB24"
                | b"RX24"
                | b"BG24"
                | b"RG24"
                | b"AB48"
                | b"AB4H"
                | b"R8  "
                | b"R16 "
                | b"AR30"
                | b"XR30"
                | b"AB30"
                | b"XB30"
        ) {
            self.to_gdk_color_state().map(Some)
        } else {
            Ok(None)
        }
    }
}

/// The real GTK window's current preferred output; unknown displays start in SDR.
pub(crate) static PREFERRED_COLOR: Lazy<watch::Sender<ColorDescription>> =
    Lazy::new(|| watch::channel(ColorDescription::srgb()).0);

#[cfg(test)]
mod tests {
    use super::ColorDescription;
    use gtk::gdk;

    #[test]
    fn yuv_dmabufs_keep_gtk_format_defaults() {
        // Includes both multiplane and packed YUV. No RGB description is
        // required, even when the surface has not negotiated a color state.
        for fourcc in [b"NV12", b"P010", b"YUYV", b"AYUV"] {
            assert!(
                ColorDescription::default()
                    .to_gdk_dmabuf_color_state(u32::from_le_bytes(*fourcc))
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn rgb_dmabufs_preserve_hdr_color_and_reject_invalid_descriptions() {
        let pq = ColorDescription {
            primaries_named: Some(6),
            tf_named: Some(11),
            ..Default::default()
        };
        for fourcc in [b"AR24", b"AB30", b"XB30", b"AB4H"] {
            let fourcc = u32::from_le_bytes(*fourcc);
            let state = pq.to_gdk_dmabuf_color_state(fourcc).unwrap().unwrap();
            assert!(state.equivalent(&gdk::ColorState::rec2100_pq()));
            assert!(
                ColorDescription::default()
                    .to_gdk_dmabuf_color_state(fourcc)
                    .is_err()
            );
        }
    }

    #[test]
    fn protocol_values_are_converted_to_cicp() {
        let pq = ColorDescription {
            primaries_named: Some(6),
            tf_named: Some(11),
            ..Default::default()
        };
        assert_eq!(pq.cicp().unwrap(), (9, 16));
        let hlg = ColorDescription {
            tf_named: Some(13),
            ..pq
        };
        assert_eq!(hlg.cicp().unwrap(), (9, 18));
        assert_eq!(ColorDescription::srgb().cicp().unwrap(), (1, 13));
    }

    #[test]
    fn unknown_and_custom_encodings_are_not_mislabeled_srgb() {
        assert!(ColorDescription::default().cicp().is_err());
        let unknown = ColorDescription {
            tf_named: Some(999),
            ..ColorDescription::srgb()
        };
        assert!(unknown.cicp().is_err());
        let custom = ColorDescription {
            primaries_named: None,
            ..ColorDescription::srgb()
        };
        assert!(custom.cicp().is_err());
    }

    #[test]
    fn sdr_fallback_has_a_nonzero_reference_white() {
        assert_eq!(ColorDescription::srgb().luminances, Some((2000, 80, 80)));
    }
}
