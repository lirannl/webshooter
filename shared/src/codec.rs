use anyhow::Result;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Av1 = 0,
    H265 = 1,
    H264 = 2,
    Vp9 = 3,
}

impl Codec {
    pub fn from_byte(b: u8) -> Result<Self> {
        Ok(match b {
            0 => Self::Av1,
            1 => Self::H265,
            2 => Self::H264,
            3 => Self::Vp9,
            d => anyhow::bail!("Invalid codec discriminant: {d}"),
        })
    }

    pub fn to_byte(self) -> u8 {
        self as u8
    }

    pub fn gst_encoder_element(self) -> &'static str {
        match self {
            Self::Av1 => "vaav1enc",
            Self::H265 => "vah265enc",
            Self::H264 => "vah264enc",
            Self::Vp9 => "vavp9enc",
        }
    }

    pub fn web_codec_string(self) -> &'static str {
        match self {
            Self::Av1 => "av01.0.09M.08",
            Self::H265 => "hvc1.1.6.L123.B0",
            Self::H264 => "avc1.640028",
            Self::Vp9 => "vp09.00.10.08",
        }
    }

    /// Priority-ordered list (best first). Server selects the first codec
    /// that the client also supports.
    pub const ALL: [Self; 4] = [Self::Av1, Self::H265, Self::H264, Self::Vp9];
}

/// Given the client's supported decoders, return the best codec to use.
pub fn select_codec(client_decoders: &[Codec]) -> Codec {
    for codec in &Codec::ALL {
        if client_decoders.iter().any(|c| c == codec) {
            return *codec;
        }
    }
    Codec::Av1
}
