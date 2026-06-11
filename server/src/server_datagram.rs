    #[repr(u8)]
    pub(crate) enum ServerDatagram<'a> {
        VideoFrame {
            frame_id: u16,
            frag_idx: u16,
            num_frags: u16,
            is_keyframe: bool,
            payload: &'a [u8],
        } = 0,
    }

    impl ServerDatagram<'_> {
        /// Fixed header overhead: 1 discriminant + 2 frame_id + 2 frag_idx + 2 num_frags + 1 flags.
        pub(crate) const HEADER: usize = 8;

        pub(crate) fn to_bytes(&self) -> Vec<u8> {
            match self {
                Self::VideoFrame {
                    frame_id,
                    frag_idx,
                    num_frags,
                    is_keyframe,
                    payload,
                } => {
                    let mut buf = Vec::with_capacity(Self::HEADER + payload.len());
                    buf.push(0u8);
                    buf.extend_from_slice(&frame_id.to_be_bytes());
                    buf.extend_from_slice(&frag_idx.to_be_bytes());
                    buf.extend_from_slice(&num_frags.to_be_bytes());
                    buf.push(*is_keyframe as u8);
                    buf.extend_from_slice(payload);
                    buf
                }
            }
        }
    }
