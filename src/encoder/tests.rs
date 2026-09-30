use super::*;

#[test]
fn bitrate_defaults_follow_resolution_tiers() {
    assert_eq!(
        bitrate_for(CaptureSize {
            width: 1920,
            height: 1080
        }),
        12_000_000
    );
    assert_eq!(
        bitrate_for(CaptureSize {
            width: 2560,
            height: 1440
        }),
        20_000_000
    );
    assert_eq!(
        bitrate_for(CaptureSize {
            width: 3840,
            height: 2160
        }),
        35_000_000
    );
}

#[test]
fn encoder_config_requires_nv12_dimensions_and_supported_fps() {
    let invalid = EncoderConfig {
        output_dir: PathBuf::from("buffer"),
        output_size: CaptureSize {
            width: 1919,
            height: 1080,
        },
        frame_rate: 60,
    };
    assert_eq!(
        validate_config(&invalid).unwrap_err().kind,
        EncoderStartErrorKind::UnsupportedFormat
    );

    // The settings screen offers 30 / 60 / 120 (task164), so all three have to
    // pass here -- 120 used to be rejected and killed the capture at startup.
    for frame_rate in [30, 60, 120] {
        let config = EncoderConfig {
            output_dir: PathBuf::from("buffer"),
            output_size: CaptureSize {
                width: 1920,
                height: 1080,
            },
            frame_rate,
        };
        assert!(
            validate_config(&config).is_ok(),
            "{frame_rate}fps must be accepted"
        );
    }

    let unsupported = EncoderConfig {
        output_dir: PathBuf::from("buffer"),
        output_size: CaptureSize {
            width: 1920,
            height: 1080,
        },
        frame_rate: 45,
    };
    assert_eq!(
        validate_config(&unsupported).unwrap_err().kind,
        EncoderStartErrorKind::UnsupportedFormat
    );
}

/// Task1760's gate, on the half nothing else in this crate touches. Encoder and
/// decoder are separately installable, so "the GPU encodes AV1" says nothing
/// about whether the review screen could ever play the result back.
///
/// This is the half that runs everywhere (task3090). It asserts that the gate
/// *agrees with* the decoder enumeration rather than that a decoder exists, so
/// a machine with no AV1 at all -- the GTX 1080 development box, which has
/// neither an AV1 encoder nor the Store `Microsoft.AV1VideoExtension` -- still
/// exercises it instead of failing on someone else's hardware. The existence
/// claim lives in `this_machine_has_an_av1_decoder_the_gate_can_see` below,
/// `#[ignore]`d because it needs that hardware.
///
/// Costs no encode session: enumeration alone, no `ActivateObject`.
///
/// It cannot go quiet the way a capability-detection skip would. Writing the
/// enumeration with the wrong category or flags makes `count_av1_decoders`
/// report zero on a machine whose gate says AV1 is available, and the
/// `decoders == 0` arm fails on exactly that.
///
/// The `decoders > 0` arm deliberately tolerates one refusal:
/// `ensure_recording_codec` checks the encoder first and returns
/// `NoHardwareEncoder` before it ever counts decoders, so "the Store extension
/// is installed on a pre-Turing GPU" is a legitimate `decoders > 0 && !supported`
/// -- one Store install away on this very machine. Any *other* refusal would
/// mean the gate misread a non-zero decoder count, which is what this arm
/// catches.
#[test]
fn the_av1_gate_agrees_with_the_decoder_enumeration() {
    let runtime = MfRuntime::start().expect("Media Foundation should initialize");
    let decoders = count_av1_decoders(&runtime).expect("AV1 decoder enumeration should succeed");
    let gate = ensure_recording_codec(VideoCodec::Av1);

    assert_eq!(
        av1_recording_supported(),
        gate.is_ok(),
        "the settings screen's cached answer must match a fresh evaluation of the same gate \
         (decoders={decoders}, gate={gate:?})"
    );

    if decoders == 0 {
        assert!(
            gate.is_err(),
            "no AV1 -> NV12 decoder was enumerated, so the gate must refuse AV1 rather than let \
             this machine record something it could never play back (decoders=0, gate={gate:?})"
        );
    } else {
        let misread = match &gate {
            Ok(()) => false,
            Err(error) => error.kind != EncoderStartErrorKind::NoHardwareEncoder,
        };
        assert!(
            !misread,
            "with {decoders} AV1 decoder(s) enumerated the gate may only refuse for want of an \
             encoder; any other refusal means it misread the decoder count (gate={gate:?})"
        );
    }
}

/// The existence claim the always-on test above cannot make: this machine
/// really does have an AV1 -> NV12 decoder MFT, so the settings screen offers
/// AV1 at all (task1730 measured it, task1760 built the gate on it).
///
/// `#[ignore]` since task3090, and *not* because it costs an encode session --
/// it still costs none. The prerequisite is hardware and an OS extension: this
/// is a statement about the acceptance machine of `docs/development.md` (RTX
/// 4070 plus the Store `Microsoft.AV1VideoExtension`), not about every machine
/// that runs the suite. The GTX 1080 development box has neither half, so this
/// test failing there is the designed outcome rather than a regression -- which
/// is why it moved off the default run instead of being deleted. It joins the
/// other opt-in AV1 tests (`hardware_encoder_produces_av1_samples`,
/// `av1_segments_reach_the_container_with_their_own_boxes`, the export probe).
///
/// ```text
/// cargo test --lib this_machine -- --ignored --nocapture
/// ```
#[test]
#[ignore = "needs an AV1 -> NV12 decoder MFT (Microsoft.AV1VideoExtension) and an AV1-capable GPU -- the RTX 4070 acceptance machine, not the GTX 1080 dev box"]
fn this_machine_has_an_av1_decoder_the_gate_can_see() {
    let runtime = MfRuntime::start().expect("Media Foundation should initialize");
    let decoders = count_av1_decoders(&runtime).expect("AV1 decoder enumeration should succeed");
    assert!(
        decoders > 0,
        "MFTEnumEx found no AV1 -> NV12 decoder; the settings screen would never offer AV1"
    );
    assert!(
        av1_recording_supported(),
        "this machine has both halves (task1730), so the settings gate must say so"
    );
}

/// AV1's half of the encoder (task1740). Needs an AV1 encoder MFT on the
/// machine; task1730 measured one here and recorded the numbers in
/// `.agents/tasks/evidence/1730-av1-feasibility/`.
///
/// Ignored because it costs an encode session, and the suite is already at the
/// driver's concurrent-session limit: measured on the RTX 4070, adding this one
/// test makes exactly one *other* hardware test fail per run, a different one
/// each time. Without it the suite is 567/567; with it, 566/567. The failure is
/// always `create_with_codec` finding no MFT that would accept the types, which
/// is what session exhaustion looks like from here.
///
/// ```text
/// cargo test --lib hardware_encoder_produces_av1_samples -- --ignored --nocapture
/// ```
#[test]
#[ignore = "task1740 AV1 encode session: the suite is at the NVENC session limit, run alone"]
fn hardware_encoder_produces_av1_samples() {
    use super::hardware::H264_PROFILE_HIGH;
    use windows::Win32::Media::MediaFoundation::{
        MFVideoFormat_AV1, MF_MT_MPEG2_PROFILE, MF_MT_SUBTYPE,
    };

    let (device, _, _) = crate::capture::create_d3d_device()
        .expect("D3D11 device should be available for the AV1 encoder");
    let config = EncoderConfig {
        output_dir: std::env::temp_dir(),
        output_size: CaptureSize {
            width: 640,
            height: 360,
        },
        frame_rate: 30,
    };
    let mut encoder = HardwareVideoEncoder::create_with_codec(&config, &device, VideoCodec::Av1)
        .expect("the hardware AV1 MFT should accept D3D11 NV12 in and AV1 out");

    let media_type = encoder.output_media_type().unwrap();
    assert_eq!(
        unsafe { media_type.GetGUID(&MF_MT_SUBTYPE) }.unwrap(),
        MFVideoFormat_AV1
    );
    // `MF_MT_MPEG2_PROFILE` is not absent here, which is worth knowing: the
    // AV1 MFT puts its own `seq_profile` in that attribute (Media Foundation
    // reuses it across codec families) on the type it advertises. What must not
    // happen is this code stamping H.264's `profile_idc` over it -- that is the
    // hand-built-type mistake task1730 warns about -- so the AV1 arm of
    // `configure_video_type` leaves whatever the MFT advertised in place.
    // Read through a `match`, not `unwrap_or_default`: absence would otherwise
    // collapse to 0, which is indistinguishable from the MFT genuinely
    // reporting seq_profile 0 (Main) -- and would satisfy the assert below
    // vacuously, in exactly the case worth noticing (task1780).
    match unsafe { media_type.GetUINT32(&MF_MT_MPEG2_PROFILE) } {
        Ok(profile) => {
            println!("AV1 output type reports MF_MT_MPEG2_PROFILE = {profile}");
            assert_ne!(
                profile, H264_PROFILE_HIGH,
                "an AV1 type must not carry H.264's High profile_idc"
            );
        }
        Err(error) => println!("AV1 output type has no MF_MT_MPEG2_PROFILE: {error}"),
    }

    let mut bytes = 0u64;
    for index in 0..45i64 {
        for sample in encoder.poll().unwrap() {
            bytes += u64::from(unsafe { sample.GetTotalLength() }.unwrap());
        }
        if encoder.has_input_credit() {
            let sample = encoder.allocate_input_sample().unwrap();
            encoder
                .process_input_sample(&sample, index * 333_333)
                .unwrap();
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    encoder.begin_drain().unwrap();
    for _ in 0..200 {
        let samples = encoder.poll().unwrap();
        if samples.is_empty() && encoder.drain_complete() {
            break;
        }
        for sample in samples {
            bytes += u64::from(unsafe { sample.GetTotalLength() }.unwrap());
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(bytes > 0, "the AV1 encoder produced no compressed bytes");
}

/// `moov` → … → `stsd` → `av01` → `av1C`, walked rather than grepped.
///
/// A flat `windows(4)` scan for the two fourccs passes on the same four bytes
/// occurring by chance inside compressed `mdat` payload, so a regression that
/// dropped the sample entry entirely would still look fine (task1780). Walking
/// the box tree is what actually says "the sink described this track as AV1".
fn assert_av1_sample_entry(bytes: &[u8]) {
    let child = |parent: &Mp4BoxEntry, kind: &[u8; 4]| {
        read_boxes(bytes, parent.payload_start, parent.end)
            .unwrap()
            .into_iter()
            .find(|entry| entry.kind == *kind)
            .unwrap_or_else(|| {
                panic!(
                    "{} must contain {}",
                    String::from_utf8_lossy(&parent.kind),
                    String::from_utf8_lossy(kind)
                )
            })
    };
    let top = read_boxes(bytes, 0, bytes.len()).unwrap();
    let moov = top
        .iter()
        .find(|entry| entry.kind == *b"moov")
        .expect("fMP4 must contain moov");
    let trak = child(moov, b"trak");
    let mdia = child(&trak, b"mdia");
    let minf = child(&mdia, b"minf");
    let stbl = child(&minf, b"stbl");
    let stsd = child(&stbl, b"stsd");
    // `stsd` is a full box: version/flags (4) + entry_count (4) precede the
    // sample entries, so the walk starts past that header.
    let entries = read_boxes(bytes, stsd.payload_start + 8, stsd.end).unwrap();
    let av01 = entries
        .iter()
        .find(|entry| entry.kind == *b"av01")
        .unwrap_or_else(|| {
            panic!(
                "stsd must describe the track as av01, found {:?}",
                entries
                    .iter()
                    .map(|entry| String::from_utf8_lossy(&entry.kind).to_string())
                    .collect::<Vec<_>>()
            )
        });
    // `av01` is a VisualSampleEntry: 78 bytes of fixed fields before its
    // children, of which `av1C` is the decoder configuration.
    let configs = read_boxes(bytes, av01.payload_start + 78, av01.end).unwrap();
    assert!(
        configs.iter().any(|entry| entry.kind == *b"av1C"),
        "the av01 sample entry must carry av1C, found {:?}",
        configs
            .iter()
            .map(|entry| String::from_utf8_lossy(&entry.kind).to_string())
            .collect::<Vec<_>>()
    );
}

/// AV1 all the way through the muxer and into a `.lvb` (task1750).
///
/// The muxer never parses the bitstream -- it hands the encoder's media type to
/// `MFCreateFMPEG4MediaSink` and the sink writes the sample entry -- so this is
/// the test that says so out loud: `av01` and `av1C` appear in the fragments
/// without a line of AV1-aware code anywhere in this crate.
///
/// Same encode-session cost as the encoder test above, so the same `#[ignore]`;
/// see the tasks README on the driver's concurrent-session limit.
///
/// ```text
/// cargo test --lib av1_segments_reach_the_container -- --ignored --nocapture
/// ```
#[test]
#[ignore = "task1750 AV1 encode session: the suite is at the NVENC session limit, run alone"]
fn av1_segments_reach_the_container_with_their_own_boxes() {
    let (device, _, _) = crate::capture::create_d3d_device()
        .expect("D3D11 device should be available for the AV1 muxing test");
    let output_dir = std::env::temp_dir().join(format!("livia-task1750-{}", std::process::id()));
    std::fs::create_dir_all(&output_dir).expect("scratch directory");
    let config = EncoderConfig {
        output_dir: output_dir.clone(),
        output_size: CaptureSize {
            width: 640,
            height: 360,
        },
        frame_rate: 30,
    };
    let mut encoder = HardwareVideoEncoder::create_with_codec(&config, &device, VideoCodec::Av1)
        .expect("the hardware AV1 MFT should start");
    let mut muxer = SegmentMuxer::new(config.clone(), encoder.output_media_type().unwrap())
        .writing_into_memory();

    let mut last_timestamp = i64::MIN;
    let mut events = Vec::new();
    // `write_video_sample_at` writes no `ctts`, so a sample out of presentation
    // order would land at the wrong time rather than be reordered. Low-latency
    // mode is what stops the encoder reordering; this is the assertion that
    // notices if it ever stops being applied to AV1.
    let take = |muxer: &mut SegmentMuxer,
                sample: &IMFSample,
                last: &mut i64,
                events: &mut Vec<EncoderEvent>| {
        let timestamp = unsafe { sample.GetSampleTime() }.expect("sample time");
        assert!(
            timestamp > *last,
            "AV1 samples must arrive in presentation order: {timestamp} came after {last}"
        );
        *last = timestamp;
        events.extend(muxer.push_video_sample(sample).expect("push AV1 sample"));
    };

    // 6 seconds of timestamps against a 2s segment. `should_force_keyframe` is
    // what actually rotates -- it is the only setter of the muxer's boundary
    // request -- so without it every frame lands in one open segment however
    // long the timeline is, and the closing-segment path never runs (task1780).
    // The capture worker pairs it with `request_next_keyframe` the same way.
    for index in 0..180i64 {
        for sample in encoder.poll().unwrap() {
            take(&mut muxer, &sample, &mut last_timestamp, &mut events);
        }
        if encoder.has_input_credit() {
            let pts = index * 333_333;
            if muxer.should_force_keyframe(pts) {
                encoder.request_next_keyframe().unwrap();
            }
            let sample = encoder.allocate_input_sample().unwrap();
            encoder.process_input_sample(&sample, pts).unwrap();
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    encoder.begin_drain().unwrap();
    for _ in 0..200 {
        let samples = encoder.poll().unwrap();
        if samples.is_empty() && encoder.drain_complete() {
            break;
        }
        for sample in samples {
            take(&mut muxer, &sample, &mut last_timestamp, &mut events);
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    events.extend(muxer.finalize().unwrap());

    // Keep the whole metadata, not just the bytes: the spans a rotation
    // produces are not uniform 2s (task201 proves that on the H.264 path), so
    // synthesizing `index * SEGMENT_DURATION` would write a container index
    // that disagrees with the fragments' own tfdt (task1780).
    let segments: Vec<&VideoSegmentMetadata> = events
        .iter()
        .filter_map(|event| match event {
            EncoderEvent::Finalized(meta) => Some(meta),
            _ => None,
        })
        .collect();
    assert!(
        segments.len() >= 2,
        "the timeline must rotate at least once so the closing-segment path is \
         exercised, got {} finalized segment(s)",
        segments.len()
    );
    for meta in &segments {
        assert_av1_sample_entry(meta.bytes.as_deref().expect("in-memory segment bytes"));
    }

    let path = output_dir.join("task1750-av1.lvb");
    let mut writer =
        crate::ring_buffer::container::ContainerWriter::create(&path, "task1750-av1", 15)
            .expect("container create");
    for meta in &segments {
        writer
            .append_segment(
                meta.index,
                meta.start_timestamp_100ns,
                meta.end_timestamp_100ns,
                &meta.audio_offsets_100ns,
                meta.bytes.as_deref().expect("in-memory segment bytes"),
            )
            .expect("append AV1 segment");
    }
    writer.close().expect("container close");

    // The same scan `--liveback-inspect` prints, so a container this rejects is
    // one the CLI would reject too.
    let dump = crate::ring_buffer::container::dump(&std::fs::read(&path).expect("read back"));
    println!("{dump}");
    assert!(
        dump.contains("segments") || dump.contains("segment"),
        "the container scan should list the AV1 segments:\n{dump}"
    );
    assert!(
        !dump.contains("truncated=true"),
        "the AV1 container must scan cleanly:\n{dump}"
    );
    // Left behind on purpose, and only reachable by running this ignored test
    // by hand: the container is the artifact worth pointing
    // `--liveback-inspect` at afterwards.
    println!("AV1 container left at {}", path.display());
}

/// task1770's two go/no-go unknowns, asked of the hardware instead of guessed.
///
/// Export's `video_fingerprint` identifies a stream by the SPS blob it reads
/// from `MF_MT_MPEG_SEQUENCE_HEADER`, and reproduces it by matching a
/// re-encoder's SPS byte for byte. Whether Media Foundation publishes anything
/// there for AV1 decides whether that mechanism has an AV1 shape at all -- the
/// AV1 config normally lives in `av1C`, and this attribute is an MPEG-family
/// one. H.264 goes first as the control, so "absent" can be told from "this
/// test asks wrong".
///
/// The second unknown is a different sink from the one task1730 measured:
/// recording writes through `MFCreateFMPEG4MediaSink`, export through
/// `MFCreateSinkWriterFromURL`. One accepting AV1 says nothing about the other.
///
/// ```text
/// cargo test --lib av1_export_probe -- --ignored --nocapture
/// ```
#[test]
#[ignore = "task1770 AV1 export probe: costs an encode session, run alone"]
fn av1_export_probe_reports_sequence_header_and_sink_writer_support() {
    use windows::Win32::Media::MediaFoundation::MF_MT_MPEG_SEQUENCE_HEADER;

    let (device, _, _) = crate::capture::create_d3d_device().expect("D3D11 device");
    let scratch = std::env::temp_dir().join(format!("livia-task1770-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).expect("scratch directory");
    let config = EncoderConfig {
        output_dir: scratch.clone(),
        output_size: CaptureSize {
            width: 640,
            height: 360,
        },
        frame_rate: 30,
    };

    let sequence_header_len = |codec: VideoCodec| -> Result<u32, String> {
        let encoder = HardwareVideoEncoder::create_with_codec(&config, &device, codec)
            .map_err(|error| error.diagnostics)?;
        encoder
            .request_next_keyframe()
            .map_err(|error| error.diagnostics)?;
        let media_type = encoder
            .output_media_type()
            .map_err(|error| error.diagnostics)?;
        unsafe { media_type.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) }
            .map_err(|error| error.to_string())
    };

    let h264 = sequence_header_len(VideoCodec::H264_HIGH);
    println!("(a) H.264 MF_MT_MPEG_SEQUENCE_HEADER: {h264:?}");
    assert!(
        h264.is_ok(),
        "the control failed: export's fingerprint reads this for H.264, so an \
         error here means the probe is wrong, not that AV1 lacks it"
    );
    let av1 = sequence_header_len(VideoCodec::Av1);
    println!("(a) AV1   MF_MT_MPEG_SEQUENCE_HEADER: {av1:?}");

    let encoder = HardwareVideoEncoder::create_with_codec(&config, &device, VideoCodec::Av1)
        .expect("AV1 encoder");
    let av1_type = encoder.output_media_type().expect("AV1 output type");
    let partial = scratch.join("av1-export-probe.mp4");
    let writer = crate::encoder::Mp4ExportWriter::create(&av1_type, &[], partial.clone());
    let accepted = writer.is_ok();
    match &writer {
        Ok(_) => println!("(b) export SinkWriter ACCEPTED an AV1 stream"),
        Err(error) => println!("(b) export SinkWriter REJECTED AV1: {}", error.diagnostics),
    }
    drop(writer);
    let _ = std::fs::remove_dir_all(&scratch);

    // Deliberately not asserted: both answers are findings task1770 acts on,
    // and a "no" here is a design input, not a broken build.
    println!("task1770 probe summary: sequence_header_av1={av1:?}, sink_writer_av1_ok={accepted}");
}

/// Migration go/no-go gate (Task016): the fMP4 sink must emit self-contained
/// fragments (ftyp/moov+mvex/moof+mdat) with a tfdt in every traf, since
/// Chromium's MSE parser requires baseMediaDecodeTime to accept an append.
#[test]
fn fmp4_segments_are_self_contained_with_tfdt_in_every_traf() {
    let (device, _, _) = crate::capture::create_d3d_device()
        .expect("D3D11 device should be available for fMP4 structural verification");
    let size = CaptureSize {
        width: 640,
        height: 360,
    };
    let output_dir = std::env::temp_dir().join(format!("livia-task016-{}", std::process::id()));
    let config = EncoderConfig {
        output_dir: output_dir.clone(),
        output_size: size,
        frame_rate: 30,
    };
    let mut encoder = HardwareVideoEncoder::create(&config, &device).unwrap();
    let aac = crate::capture::audio::AacEncoder::create().expect("system AAC MFT");
    let mut muxer = SegmentMuxer::with_aac(
        config.clone(),
        encoder.output_media_type().unwrap(),
        aac.output_media_type().clone(),
    );
    let mut encoded_count = 0;
    let mut pending_aac = crate::encoder::PendingAac::default();
    for index in 0..45i64 {
        for sample in encoder.poll().unwrap() {
            muxer.push_video_sample(&sample).unwrap();
            encoded_count += 1;
        }
        let pcm = vec![0.0_f32; 1024 * 2];
        for sample in aac.encode_f32(&pcm, index * 166_667).unwrap() {
            pending_aac.offer(&mut muxer, &sample);
        }
        if encoder.has_input_credit() {
            let sample = encoder.allocate_input_sample().unwrap();
            encoder
                .process_input_sample(&sample, index * 333_333)
                .unwrap();
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    encoder.begin_drain().unwrap();
    for _ in 0..200 {
        let samples = encoder.poll().unwrap();
        if samples.is_empty() && encoder.drain_complete() {
            break;
        }
        for sample in samples {
            muxer.push_video_sample(&sample).unwrap();
            encoded_count += 1;
        }
        pending_aac.flush(&mut muxer);
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    pending_aac.assert_drained();
    assert!(
        encoded_count > 0,
        "hardware MFT must emit at least one H.264 sample"
    );
    let mut events = muxer.finalize().unwrap();
    assert_eq!(
        events.len(),
        1,
        "a single segment should have opened and finalized"
    );
    let event = events.remove(0);
    let path = match event {
        EncoderEvent::Finalized(meta) => meta.path,
        other => panic!("expected Finalized event, got {other:?}"),
    };
    let bytes = std::fs::read(&path).expect("finalized fMP4 segment should be readable");

    let top = read_boxes(&bytes, 0, bytes.len()).unwrap();
    assert!(
        top.iter().any(|entry| entry.kind == *b"ftyp"),
        "fMP4 must start with an ftyp box"
    );
    let moov = top
        .iter()
        .find(|entry| entry.kind == *b"moov")
        .expect("fMP4 must contain a moov box");
    let moov_children = read_boxes(&bytes, moov.payload_start, moov.end).unwrap();
    assert!(
        moov_children.iter().any(|entry| entry.kind == *b"mvex"),
        "moov must contain mvex to mark the file as fragmented"
    );
    let moofs: Vec<_> = top.iter().filter(|entry| entry.kind == *b"moof").collect();
    assert!(
        !moofs.is_empty(),
        "fragmented MP4 must contain at least one top-level moof"
    );
    let mut traf_count = 0;
    let mut tfdt_count = 0;
    let mut default_base_is_moof_count = 0;
    for moof in &moofs {
        // This muxer always writes exactly one mdat immediately after each
        // moof (interleaved multi-track samples share it), which is what lets
        // this test check trun.data_offset without re-deriving it.
        let mdat = top
            .iter()
            .find(|entry| entry.start >= moof.end && entry.kind == *b"mdat")
            .expect("moof must be followed by an mdat");
        let moof_children = read_boxes(&bytes, moof.payload_start, moof.end).unwrap();
        for traf in moof_children.iter().filter(|entry| entry.kind == *b"traf") {
            traf_count += 1;
            let traf_children = read_boxes(&bytes, traf.payload_start, traf.end).unwrap();
            if traf_children.iter().any(|entry| entry.kind == *b"tfdt") {
                tfdt_count += 1;
            }
            let tfhd = traf_children
                .iter()
                .find(|entry| entry.kind == *b"tfhd")
                .expect("traf must contain tfhd");
            let tfhd_info = parse_tfhd(&bytes, tfhd).unwrap();
            let version_flags = read_u32(&bytes, tfhd.payload_start).unwrap();
            if version_flags & 0x0002_0000 != 0 {
                default_base_is_moof_count += 1;
            }
            assert!(
                version_flags & 0x0000_0001 == 0,
                "tfhd must not use base-data-offset-present; MSE requires \
                 default-base-is-moof (see moof at byte {})",
                moof.start
            );
            for trun in traf_children.iter().filter(|entry| entry.kind == *b"trun") {
                let trun_info =
                    parse_trun(&bytes, trun, tfhd_info.default_sample_duration).unwrap();
                if let Some(field_at) = trun_info.data_offset_field_at {
                    let data_offset = read_u32(&bytes, field_at).unwrap() as i32 as i64;
                    let absolute = moof.start as i64 + data_offset;
                    assert!(
                        absolute >= mdat.payload_start as i64 && absolute < mdat.end as i64,
                        "trun.data_offset (moof-relative) must resolve inside this \
                         fragment's own mdat: moof.start={} data_offset={} resolved={} \
                         mdat=[{},{})",
                        moof.start,
                        data_offset,
                        absolute,
                        mdat.payload_start,
                        mdat.end
                    );
                }
            }
        }
    }
    assert!(traf_count > 0, "every moof must contain at least one traf");
    assert_eq!(
        tfdt_count, traf_count,
        "every traf must contain a tfdt (baseMediaDecodeTime) box for Chromium MSE \
         compatibility; if this fails, MFCreateFMPEG4MediaSink omits tfdt and a \
         finalize-time tfdt-injection post-processor must be implemented"
    );
    assert_eq!(
        default_base_is_moof_count, traf_count,
        "every tfhd must set default-base-is-moof; MSE disallows base-data-offset-present"
    );
    assert!(
        top.iter().any(|entry| entry.kind == *b"mdat"),
        "fragmented MP4 must contain at least one mdat box"
    );
    let inspection = inspect_finalized_mp4(&path)
        .expect("MF Source Reader should still decode the rewritten fMP4 segment");
    assert!(
        inspection.sample_count > 0,
        "rewritten segment must retain decodable video samples"
    );
    assert!(
        inspection.has_aac_audio,
        "rewritten segment must retain decodable AAC audio"
    );

    if std::env::var_os("LIVIA_KEEP_ACCEPTANCE_OUTPUT").is_none() {
        let _ = std::fs::remove_dir_all(output_dir);
    } else {
        eprintln!("kept task016 output: {}", path.display());
    }
}

/// Task201 regression: the fMP4 sink derives a sample's duration from the
/// *next* sample's time and only falls back to the stamped `SetSampleDuration`
/// for the last sample in a fragment. The encoder stamps every input with a
/// nominal 1/frame_rate, so that last sample used to claim 1/30s no matter how
/// long the frame was really on screen -- and since the next fragment's tfdt is
/// the running sum of durations, every fragment lost the difference. A segment
/// declaring 1.97s held only 1.83s of internal timeline.
///
/// Feeds deliberately uneven intervals (a still target's real pattern: WGC only
/// delivers on change) and checks the durations the file ends up with are the
/// measured gaps, not the nominal one.
#[test]
fn task201_sample_durations_follow_the_real_frame_intervals() {
    let (device, _, _) = crate::capture::create_d3d_device()
        .expect("D3D11 device should be available for duration verification");
    let size = CaptureSize {
        width: 640,
        height: 360,
    };
    let output_dir = std::env::temp_dir().join(format!("livia-task201-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&output_dir);
    let config = EncoderConfig {
        output_dir: output_dir.clone(),
        output_size: size,
        frame_rate: 30, // nominal 333_333 per input sample
    };
    let mut encoder = HardwareVideoEncoder::create(&config, &device).unwrap();
    let mut muxer = SegmentMuxer::new(config.clone(), encoder.output_media_type().unwrap());

    // Intervals nothing like 1/30s, and deliberately uneven.
    let intervals_100ns: [i64; 11] = [
        500_000, 500_000, 2_000_000, 170_000, 1_200_000, 800_000, 300_000, 300_000, 900_000,
        450_000, 600_000,
    ];
    let mut wanted = vec![0i64];
    for interval in intervals_100ns {
        wanted.push(wanted.last().unwrap() + interval);
    }
    // Only what the encoder actually took: under load it can be out of input
    // credit at any of these moments, and assuming otherwise made this test
    // fail in a full-suite run for a reason that had nothing to do with
    // durations.
    let mut submitted: Vec<i64> = Vec::new();
    for timestamp in &wanted {
        for sample in encoder.poll().unwrap() {
            muxer.push_video_sample(&sample).unwrap();
        }
        if encoder.has_input_credit() {
            let sample = encoder.allocate_input_sample().unwrap();
            encoder.process_input_sample(&sample, *timestamp).unwrap();
            submitted.push(*timestamp);
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    encoder.begin_drain().unwrap();
    for _ in 0..200 {
        let samples = encoder.poll().unwrap();
        if samples.is_empty() && encoder.drain_complete() {
            break;
        }
        for sample in samples {
            muxer.push_video_sample(&sample).unwrap();
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let mut events = muxer.finalize().unwrap();
    assert_eq!(events.len(), 1, "one segment should have opened");
    let path = match events.remove(0) {
        EncoderEvent::Finalized(meta) => meta.path,
        other => panic!("expected Finalized, got {other:?}"),
    };

    let summary = dump_segment_track_summary(&path).expect("track summary reads");
    let video = summary
        .iter()
        .find(|track| !track.is_audio)
        .expect("a video track");
    assert_eq!(
        video.sample_count as usize,
        submitted.len(),
        "every accepted input should appear in the file"
    );
    // The file's total timeline must equal the span the encoder was fed plus
    // the last frame's own nominal duration -- the only one not measurable
    // from inside the segment.
    let nominal_100ns = 10_000_000 / i64::from(config.frame_rate);
    let expected_100ns = submitted.last().copied().unwrap_or(0)
        - submitted.first().copied().unwrap_or(0)
        + nominal_100ns;
    let actual_100ns =
        video.total_sample_duration_ticks * 10_000_000 / i64::from(video.timescale.max(1));
    assert!(
        (actual_100ns - expected_100ns).abs() <= nominal_100ns,
        "the file's video timeline must match the intervals actually submitted: \
         expected {expected_100ns} (span {} + one nominal frame), got {actual_100ns}, \
         over {} samples",
        submitted.last().copied().unwrap_or(0) - submitted.first().copied().unwrap_or(0),
        video.sample_count,
    );
    if std::env::var_os("LIVIA_KEEP_ACCEPTANCE_OUTPUT").is_none() {
        let _ = std::fs::remove_dir_all(output_dir);
    }
}

/// Task201, the rotation half: a segment that is closed by a rotation gets its
/// last sample's duration from the *next* segment's opening clean point, which
/// is the only place that interval exists. Runs long enough to rotate several
/// times and checks every finalized segment's internal timeline against the
/// span its own metadata declares -- the comparison the manifest's
/// `end - start` makes for a real recording.
#[test]
fn task201_every_rotated_segment_holds_the_span_it_declares() {
    let (device, _, _) = crate::capture::create_d3d_device()
        .expect("D3D11 device should be available for duration verification");
    let size = CaptureSize {
        width: 640,
        height: 360,
    };
    let output_dir = std::env::temp_dir().join(format!("livia-task201r-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&output_dir);
    let config = EncoderConfig {
        output_dir: output_dir.clone(),
        output_size: size,
        frame_rate: 60,
    };
    let nominal_100ns = 10_000_000 / i64::from(config.frame_rate);
    let mut encoder = HardwareVideoEncoder::create(&config, &device).unwrap();
    let mut muxer = SegmentMuxer::new(config.clone(), encoder.output_media_type().unwrap());

    // Uneven, and mostly far longer than 1/60s -- a still target's real
    // pattern, and the case the nominal duration got wrong.
    let intervals_100ns = [400_000i64, 1_500_000, 250_000, 900_000, 3_000_000, 600_000];
    let mut finalized = Vec::new();
    let mut timestamp = 0i64;
    // ~8s of media time at these intervals: enough for several 2s rotations.
    while timestamp < 8 * 10_000_000 {
        finalized.extend(encoder.poll_to_muxer(&mut muxer).unwrap());
        if encoder.has_input_credit() {
            if muxer.should_force_keyframe(timestamp) {
                encoder.request_next_keyframe().unwrap();
            }
            let sample = encoder.allocate_input_sample().unwrap();
            encoder.process_input_sample(&sample, timestamp).unwrap();
            timestamp += intervals_100ns[finalized.len() % intervals_100ns.len()];
        } else {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
    encoder.begin_drain().unwrap();
    for _ in 0..5_000 {
        finalized.extend(encoder.poll_to_muxer(&mut muxer).unwrap());
        if encoder.drain_complete() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    finalized.extend(muxer.finalize().unwrap());

    let segments: Vec<_> = finalized
        .into_iter()
        .filter_map(|event| match event {
            EncoderEvent::Finalized(meta) => Some(meta),
            _ => None,
        })
        .collect();
    assert!(
        segments.len() >= 3,
        "expected several rotations, got {} segment(s)",
        segments.len()
    );
    for meta in &segments {
        let summary = dump_segment_track_summary(&meta.path).expect("track summary reads");
        let video = summary
            .iter()
            .find(|track| !track.is_audio)
            .expect("a video track");
        let actual_100ns =
            video.total_sample_duration_ticks * 10_000_000 / i64::from(video.timescale.max(1));
        // What the catalog will declare for this segment: last sample plus one
        // nominal frame (`exclusive_segment_end`).
        let declared_100ns = meta.end_timestamp_100ns - meta.start_timestamp_100ns + nominal_100ns;
        assert!(
            (actual_100ns - declared_100ns).abs() <= nominal_100ns,
            "segment {} holds {actual_100ns} of timeline but declares {declared_100ns} \
             (start {}, end {})",
            meta.index,
            meta.start_timestamp_100ns,
            meta.end_timestamp_100ns,
        );
    }

    if std::env::var_os("LIVIA_KEEP_ACCEPTANCE_OUTPUT").is_none() {
        let _ = std::fs::remove_dir_all(output_dir);
    }
}

/// Task204: every segment after the first must open with a copy of the
/// previous segment's last AAC access unit, so its decoder has the predecessor
/// each block is overlap-added with. Without it the first 21ms of every segment
/// decodes as a fade-in from near silence -- measured at -3.0dB over the block,
/// ramping monotonically to level, at every seam of a real recording.
#[test]
fn task204_each_segment_after_the_first_opens_with_a_priming_access_unit() {
    let (device, _, _) = crate::capture::create_d3d_device()
        .expect("D3D11 device should be available for priming verification");
    let size = CaptureSize {
        width: 640,
        height: 360,
    };
    let output_dir = std::env::temp_dir().join(format!("livia-task204-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&output_dir);
    let config = EncoderConfig {
        output_dir: output_dir.clone(),
        output_size: size,
        frame_rate: 30,
    };
    let mut encoder = HardwareVideoEncoder::create(&config, &device).unwrap();
    let aac = crate::capture::audio::AacEncoder::create().expect("system AAC MFT");
    let mut muxer = SegmentMuxer::with_aac(
        config.clone(),
        encoder.output_media_type().unwrap(),
        aac.output_media_type().clone(),
    );
    // Long enough to rotate at least once (segments are 2s).
    let mut finalized = Vec::new();
    let mut pending_aac = crate::encoder::PendingAac::default();
    for index in 0..160i64 {
        finalized.extend(encoder.poll_to_muxer(&mut muxer).unwrap());
        let pcm = vec![0.25_f32; 1024 * 2];
        for sample in aac.encode_f32(&pcm, index * 333_333).unwrap() {
            finalized.extend(pending_aac.offer(&mut muxer, &sample));
        }
        if encoder.has_input_credit() {
            let timestamp = index * 333_333;
            if muxer.should_force_keyframe(timestamp) {
                encoder.request_next_keyframe().unwrap();
            }
            let sample = encoder.allocate_input_sample().unwrap();
            encoder.process_input_sample(&sample, timestamp).unwrap();
        }
        // Paced like the real capture loop, which is driven by WASAPI's ~10ms
        // period. Pushing a whole recording's worth of AAC as fast as the loop
        // runs outpaces the sink's queue and fails with MF_E_NOTACCEPTING under
        // load -- a property of the test, not of the muxer.
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    encoder.begin_drain().unwrap();
    for _ in 0..5_000 {
        finalized.extend(encoder.poll_to_muxer(&mut muxer).unwrap());
        finalized.extend(pending_aac.flush(&mut muxer));
        if encoder.drain_complete() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    pending_aac.assert_drained();
    finalized.extend(muxer.finalize().unwrap());

    let segments: Vec<_> = finalized
        .into_iter()
        .filter_map(|event| match event {
            EncoderEvent::Finalized(meta) => Some(meta),
            _ => None,
        })
        .collect();
    assert!(
        segments.len() >= 2,
        "expected a rotation, got {} segment(s)",
        segments.len()
    );
    assert_eq!(
        segments[0].audio_offsets_100ns,
        vec![0],
        "the first segment has no predecessor to prime from"
    );
    for meta in &segments[1..] {
        assert!(
            meta.audio_offsets_100ns[0] > 0,
            "segment {} carries no priming offset",
            meta.index
        );
        // The offset is the distance from the repeated unit to the video clean
        // point that opened the segment. AAC lags video through the encoder, so
        // it is normally a few tens of milliseconds rather than exactly one
        // access unit -- the unit is still the immediate predecessor of this
        // segment's first real one, which is all the decoder needs. Bounded
        // only loosely, to catch a stale unit being carried across a rotation.
        assert!(
            meta.audio_offsets_100ns[0] < SEGMENT_DURATION_100NS,
            "segment {} shifted its audio by {}, which is a whole segment or more",
            meta.index,
            meta.audio_offsets_100ns[0],
        );
        let durations = mp4_boxes::dump_audio_sample_durations(&meta.path).expect("durations");
        assert!(
            !durations.is_empty(),
            "segment {} has no audio at all",
            meta.index
        );
    }

    if std::env::var_os("LIVIA_KEEP_ACCEPTANCE_OUTPUT").is_none() {
        let _ = std::fs::remove_dir_all(output_dir);
    }
}

/// Task062 regression: before this task, AAC samples arriving after a
/// segment rotation but still timestamped before it (WASAPI/AAC encode
/// latency) were silently discarded (`encoder.rs` used to `return Ok(())`
/// unconditionally once `timestamp < open.start_timestamp_100ns`). Drives
/// video across two segment boundaries (opening at least 3 segments) while
/// interleaving AAC pushes exactly like the real capture loop does, and
/// checks every opened segment finalizes exactly once, in index order, with
/// zero audio dropped for having "missed" its `closing` slot.
#[test]
fn late_audio_across_a_segment_rotation_is_never_silently_dropped() {
    let (device, _, _) = crate::capture::create_d3d_device()
        .expect("D3D11 device should be available for closing-writer verification");
    let size = CaptureSize {
        width: 320,
        height: 180,
    };
    let output_dir =
        std::env::temp_dir().join(format!("livia-task062-closing-{}", std::process::id()));
    let config = EncoderConfig {
        output_dir: output_dir.clone(),
        output_size: size,
        frame_rate: 30,
    };
    let mut encoder = HardwareVideoEncoder::create(&config, &device).unwrap();
    let aac = crate::capture::audio::AacEncoder::create().expect("system AAC MFT");
    let mut muxer = SegmentMuxer::with_aac(
        config.clone(),
        encoder.output_media_type().unwrap(),
        aac.output_media_type().clone(),
    );

    let frame_duration_100ns = 333_333i64; // ~30fps
    let mut events = Vec::new();
    let mut pending_aac = crate::encoder::PendingAac::default();
    let mut timestamp = 0i64;
    // Comfortably over two full 2s boundaries: proves `closing` is always
    // finalized before a third rotation would need to reuse the slot.
    while timestamp < 43_000_000 {
        for sample in encoder.poll().unwrap() {
            events.extend(muxer.push_video_sample(&sample).unwrap());
        }
        let pcm = vec![0.0_f32; 1024 * 2];
        for sample in aac.encode_f32(&pcm, timestamp).unwrap() {
            events.extend(pending_aac.offer(&mut muxer, &sample));
        }
        if encoder.has_input_credit() {
            if muxer.should_force_keyframe(timestamp) {
                encoder.request_next_keyframe().unwrap();
            }
            let sample = encoder.allocate_input_sample().unwrap();
            encoder.process_input_sample(&sample, timestamp).unwrap();
            timestamp += frame_duration_100ns;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    encoder.begin_drain().unwrap();
    for _ in 0..500 {
        let samples = encoder.poll().unwrap();
        if samples.is_empty() && encoder.drain_complete() {
            break;
        }
        for sample in samples {
            events.extend(muxer.push_video_sample(&sample).unwrap());
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    let opened_segments = muxer.next_index;
    assert_eq!(
        muxer.dropped_audio_after_close(),
        0,
        "no AAC sample should ever find `closing` already finalized during a \
         normal run: every rotation finalizes the previous `closing` before \
         creating a new one, so a sample belonging to it always has somewhere \
         to land"
    );
    assert!(
        muxer.dropped_audio_before_open() < 10,
        "only samples arriving before the very first video clean point should \
         ever hit this counter, and only for a frame or two: got {}",
        muxer.dropped_audio_before_open()
    );
    events.extend(muxer.finalize().unwrap());

    assert!(
        opened_segments >= 3,
        "test must actually cross at least two segment boundaries (opened \
         {opened_segments} segments)"
    );

    let finalized_indices: Vec<u64> = events
        .iter()
        .filter_map(|event| match event {
            EncoderEvent::Finalized(meta) => Some(meta.index),
            _ => None,
        })
        .collect();
    assert_eq!(
        finalized_indices.len() as u64,
        opened_segments,
        "every opened segment must finalize exactly once: {finalized_indices:?}"
    );
    assert!(
        finalized_indices.windows(2).all(|pair| pair[0] < pair[1]),
        "Finalized events must be emitted in strictly increasing index order: \
         {finalized_indices:?}"
    );

    if std::env::var_os("LIVIA_KEEP_ACCEPTANCE_OUTPUT").is_none() {
        let _ = std::fs::remove_dir_all(&output_dir);
    } else {
        eprintln!("kept task062 output: {}", output_dir.display());
    }
}

/// Task088 Step1: per-sample audio duration distribution for one real
/// segment, to tell apart "samples are individually present at the
/// nominal AAC frame length but too few of them exist" (frame drop) from
/// "each sample's own declared duration is inflated beyond the nominal
/// AAC frame length" (duration/timestamp miscalculation) -- a summed
/// per-track total cannot distinguish the two. Point `LIVIA_TASK088_VERIFY_SEGMENT` at one
/// `.mp4` segment file to run this.
#[test]
fn task088_dumps_audio_sample_duration_distribution_when_requested() {
    let Ok(path) = std::env::var("LIVIA_TASK088_VERIFY_SEGMENT") else {
        return;
    };
    let durations = mp4_boxes::dump_audio_sample_durations(std::path::Path::new(&path))
        .unwrap_or_else(|error| panic!("{path}: {error}"));
    assert!(!durations.is_empty(), "segment has no audio samples");
    const NOMINAL_AAC_FRAME_TICKS: u32 = 1024; // 1024/48000s at 48kHz timescale
    let min = *durations.iter().min().unwrap();
    let max = *durations.iter().max().unwrap();
    let sum: u64 = durations.iter().map(|value| u64::from(*value)).sum();
    let mean = sum as f64 / durations.len() as f64;
    let at_nominal = durations
        .iter()
        .filter(|value| **value == NOMINAL_AAC_FRAME_TICKS)
        .count();
    println!(
        "Task088 audio sample duration distribution: segment={path}, \
         sample_count={}, nominal_ticks={NOMINAL_AAC_FRAME_TICKS}, \
         at_nominal={at_nominal}, min={min}, max={max}, mean={mean:.1}, \
         first_20={:?}",
        durations.len(),
        &durations[..durations.len().min(20)],
    );
}

/// Task420: a segment written into memory and appended to a `.lvb` container
/// must be the same bytes a `.partial.mp4` would have held.
///
/// Both writers are driven from **the same encoder output in the same loop**,
/// for the reason task402 established: two separate hardware encoder runs
/// disagree on sample count by themselves, so a difference would prove
/// nothing. The only variable is where the sink writes.
///
/// Task450 made the container the only shape recording writes, so the append
/// below is the production call (`src/capture/indexer.rs`), not a seam.
#[test]
fn task420_a_container_segment_matches_the_file_segment_byte_for_byte() {
    use crate::ring_buffer::container::{ContainerReader, ContainerWriter};

    let (device, _, _) = crate::capture::create_d3d_device()
        .expect("D3D11 device should be available for the container write path");
    let size = CaptureSize {
        width: 640,
        height: 360,
    };
    let output_dir = std::env::temp_dir().join(format!("livia-task420-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&output_dir);
    std::fs::create_dir_all(&output_dir).unwrap();
    let config = EncoderConfig {
        output_dir: output_dir.clone(),
        output_size: size,
        frame_rate: 30,
    };

    let mut encoder = HardwareVideoEncoder::create(&config, &device).unwrap();
    let h264 = encoder.output_media_type().unwrap();
    let aac_encoder = crate::capture::audio::AacEncoder::create().expect("system AAC MFT");
    let aac = aac_encoder.output_media_type().clone();

    // Same media types, same index, same samples -- one to a file, one to memory.
    let file_writer =
        Mp4SegmentWriter::create_with_audio(&config, &h264, Some(&aac), 1, 0).unwrap();
    let memory_writer = Mp4SegmentWriter::create_in_memory(&config, &h264, Some(&aac), 1).unwrap();

    // Segment-relative, as the muxer rebases them: the first sample sits at 0.
    let mut first_video_100ns: Option<i64> = None;
    let mut relative = |sample: &IMFSample| -> i64 {
        let absolute = unsafe { sample.GetSampleTime() }.unwrap_or(0);
        let first = *first_video_100ns.get_or_insert(absolute);
        absolute - first
    };
    let mut video_samples = 0;
    for index in 0..45i64 {
        for sample in encoder.poll().unwrap() {
            let at = relative(&sample);
            assert!(file_writer.write_video_sample_at(&sample, at).unwrap());
            assert!(memory_writer.write_video_sample_at(&sample, at).unwrap());
            video_samples += 1;
        }
        let pcm = vec![0.0_f32; 1024 * 2];
        for sample in aac_encoder.encode_f32(&pcm, index * 166_667).unwrap() {
            file_writer
                .write_aac_sample_at(&sample, index * 166_667)
                .unwrap();
            memory_writer
                .write_aac_sample_at(&sample, index * 166_667)
                .unwrap();
        }
        if encoder.has_input_credit() {
            let sample = encoder.allocate_input_sample().unwrap();
            encoder
                .process_input_sample(&sample, index * 333_333)
                .unwrap();
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    encoder.begin_drain().unwrap();
    for _ in 0..200 {
        let samples = encoder.poll().unwrap();
        if samples.is_empty() && encoder.drain_complete() {
            break;
        }
        for sample in samples {
            let at = relative(&sample);
            assert!(file_writer.write_video_sample_at(&sample, at).unwrap());
            assert!(memory_writer.write_video_sample_at(&sample, at).unwrap());
            video_samples += 1;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(
        video_samples > 0,
        "the hardware MFT emitted no H.264 samples"
    );

    let file_path = file_writer.finalize().unwrap();
    let from_file = std::fs::read(&file_path).unwrap();
    let from_memory = memory_writer.finalize_into_bytes().unwrap();

    assert_eq!(
        from_memory.len(),
        from_file.len(),
        "the in-memory segment is a different size from the file one"
    );
    assert_eq!(
        from_memory, from_file,
        "the in-memory segment differs from the file one byte for byte"
    );

    // ...and those bytes survive a round trip through the container, together
    // with a thumbnail appended the way the capture worker would.
    let jpeg = vec![0xFFu8, 0xD8, 0x00, 0x11, 0x22];
    let container_path = output_dir.join("session.lvb");
    let mut writer = ContainerWriter::create(&container_path, "capture-task420", 15).unwrap();
    writer
        .append_segment(0, 0, 20_000_000, &[0], &from_memory)
        .unwrap();
    writer.append_thumbnail(0, &jpeg).unwrap();
    writer.close().unwrap();

    let mut reader = ContainerReader::open(&container_path).unwrap();
    assert!(!reader.needs_recovery());
    assert_eq!(
        reader.read_segment(0).unwrap(),
        from_file,
        "the segment did not survive the container round trip"
    );
    assert_eq!(reader.read_thumbnail(0).unwrap(), jpeg);

    // Kept only when asked, so `--liveback-inspect` has something real to be
    // pointed at (the task's Verification). Same affordance task401's sample
    // writer has.
    if std::env::var_os("LIVIA_TASK420_KEEP").is_some() {
        println!("container={}", container_path.display());
    } else {
        let _ = std::fs::remove_dir_all(&output_dir);
    }
}

/// Task610. The recording used to die when a stalled capture thread meant the
/// last AAC access unit was seconds old at the next segment rotation: it was
/// repeated at local 0 and the whole audio track shifted by its age, which put
/// the segment's first real sample seconds ahead of its video, which the fMP4
/// sink refuses -- and the refusal killed the capture. Measured ages at the
/// deaths were 0.28s to 7.4s; a genuine priming unit is one AAC-LC block, 21ms.
#[test]
fn a_stale_aac_priming_unit_is_not_this_segments_predecessor() {
    let block_100ns = 213_333; // 1024 samples at 48kHz
    let start = 1_000_000_000;

    assert!(
        super::priming_is_fresh(start, start - block_100ns),
        "the unit immediately before the rotation is exactly what priming is for"
    );
    assert!(super::priming_is_fresh(start, start), "a zero gap is fresh");
    assert!(
        super::priming_is_fresh(start, start + block_100ns),
        "a unit stamped slightly after the rotation is still adjacent, not stale"
    );
    assert!(
        super::priming_is_fresh(start, start - super::MAX_PRIMING_AGE_100NS),
        "the limit itself is still fresh"
    );
    assert!(
        !super::priming_is_fresh(start, start - super::MAX_PRIMING_AGE_100NS - 1),
        "one tick past the limit is stale"
    );
    // The two clusters actually measured while reproducing the death.
    for age_100ns in [2_785_316, 13_842_906, 73_981_799] {
        assert!(
            !super::priming_is_fresh(start, start - age_100ns),
            "{age_100ns} 100ns is a gap the capture thread stalled through, not priming"
        );
    }
}

/// Task1260: a segment writer with three audio tracks has to keep them apart.
///
/// Each track is given a **different number of samples**, so a writer that
/// quietly routed everything to track 0 (or dropped the extras) cannot pass:
/// the counts that come back have to be the counts that went in, per track.
/// The read-back opens one Source Reader per track for the reason task1240
/// established -- one reader is one media source, and reading track 0 to
/// end-of-stream leaves every later track answering EOS immediately.
#[test]
fn task1260_a_segment_keeps_its_audio_tracks_apart() {
    use windows::Win32::Media::MediaFoundation::*;

    let (device, _, _) = crate::capture::create_d3d_device().expect("D3D11 device");
    let size = CaptureSize {
        width: 640,
        height: 360,
    };
    let output_dir = std::env::temp_dir().join(format!("livia-task1260-{}", std::process::id()));
    std::fs::create_dir_all(&output_dir).unwrap();
    let config = EncoderConfig {
        output_dir: output_dir.clone(),
        output_size: size,
        frame_rate: 30,
    };
    let mut encoder = HardwareVideoEncoder::create(&config, &device).unwrap();
    let h264 = encoder.output_media_type().unwrap();
    let aac_encoder = crate::capture::audio::AacEncoder::create().expect("system AAC MFT");
    let aac = aac_encoder.output_media_type().clone();

    const TRACKS: usize = 3;
    // Track 0 gets every AAC frame, track 1 every second one, track 2 every
    // third: three counts that cannot be confused with each other.
    let strides = [1usize, 2, 3];

    let writer =
        Mp4SegmentWriter::create_with_audio(&config, &h264, Some(&aac), TRACKS, 0).unwrap();

    let mut written = [0u32; TRACKS];
    let mut frame = 0usize;
    for index in 0..45i64 {
        for sample in encoder.poll().unwrap() {
            let timestamp = unsafe { sample.GetSampleTime().unwrap_or(index * 333_333) };
            assert!(writer.write_video_sample_at(&sample, timestamp).unwrap());
        }
        let pcm = vec![0.0_f32; 1024 * 2];
        for sample in aac_encoder.encode_f32(&pcm, index * 166_667).unwrap() {
            let timestamp = unsafe { sample.GetSampleTime().unwrap_or(index * 166_667) };
            for (track, stride) in strides.iter().enumerate() {
                if !frame.is_multiple_of(*stride) {
                    continue;
                }
                // A refusal here would skew the counts, so retry rather than
                // silently writing fewer: this is a fresh sink with nothing to
                // back up behind.
                for _ in 0..50 {
                    if writer
                        .write_aac_sample_on(track, &sample, timestamp)
                        .unwrap()
                    {
                        written[track] += 1;
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
            frame += 1;
        }
        if encoder.has_input_credit() {
            let sample = encoder.allocate_input_sample().unwrap();
            encoder
                .process_input_sample(&sample, index * 333_333)
                .unwrap();
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    encoder.begin_drain().unwrap();
    for _ in 0..200 {
        let samples = encoder.poll().unwrap();
        if samples.is_empty() && encoder.drain_complete() {
            break;
        }
        for sample in samples {
            let timestamp = unsafe { sample.GetSampleTime().unwrap_or_default() };
            assert!(writer.write_video_sample_at(&sample, timestamp).unwrap());
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let path = writer.finalize().unwrap();
    println!("task1260: wrote {written:?} AAC samples per track");
    assert!(written[0] > written[1] && written[1] > written[2] && written[2] > 0);

    let mut read_back = Vec::new();
    unsafe {
        let url = windows::core::HSTRING::from(path.to_string_lossy().as_ref());
        let probe = MFCreateSourceReaderFromURL(&url, None).expect("the segment opens as media");
        let mut audio_streams = Vec::new();
        for index in 0..32u32 {
            let Ok(native) = probe.GetNativeMediaType(index, 0) else {
                break;
            };
            if native.GetGUID(&MF_MT_MAJOR_TYPE).unwrap() == MFMediaType_Audio {
                audio_streams.push(index);
            }
        }
        drop(probe);
        assert_eq!(
            audio_streams.len(),
            TRACKS,
            "the segment should carry one audio track per writer track"
        );
        for index in audio_streams {
            let reader = MFCreateSourceReaderFromURL(&url, None).unwrap();
            for other in 0..32u32 {
                if reader.GetNativeMediaType(other, 0).is_err() {
                    break;
                }
                let _ = reader.SetStreamSelection(other, other == index);
            }
            let mut samples = 0u32;
            for _ in 0..4000 {
                let mut flags = 0;
                let mut timestamp = 0;
                let mut sample = None;
                reader
                    .ReadSample(
                        index,
                        0,
                        None,
                        Some(&mut flags),
                        Some(&mut timestamp),
                        Some(&mut sample),
                    )
                    .unwrap();
                if sample.is_some() {
                    samples += 1;
                }
                if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                    break;
                }
            }
            read_back.push(samples);
        }
    }
    println!("task1260: read back {read_back:?} samples per track");

    // **Track order lives in the track ids, not in the reader stream order.**
    // The Source Reader hands the audio streams back in the *reverse* of the
    // order they were added (measured: writer tracks 0,1,2 come back as reader
    // streams 2,1,0), while the mp4 numbers the traks in add order. Everything
    // that has to know which track is which -- the tfdt injection here, the
    // mixer later -- goes by ascending track id.
    let mut tracks =
        crate::encoder::inspect::dump_segment_track_summary(&path).expect("track summary");
    tracks.sort_by_key(|track| track.track_id);
    println!("task1260: box tracks {tracks:?}");
    let audio_counts: Vec<u64> = tracks
        .iter()
        .filter(|track| track.is_audio)
        .map(|track| track.sample_count)
        .collect();
    assert_eq!(
        audio_counts,
        written
            .iter()
            .map(|count| u64::from(*count))
            .collect::<Vec<_>>(),
        "ascending audio track ids must carry the writer track order"
    );

    // And every track is genuinely decodable, not merely present in the boxes.
    let mut sorted_read_back = read_back.clone();
    sorted_read_back.sort_unstable();
    let mut sorted_written = written.to_vec();
    sorted_written.sort_unstable();
    assert_eq!(
        sorted_read_back, sorted_written,
        "tracks came back as {read_back:?}, which is not the {written:?} that went in"
    );

    let _ = std::fs::remove_dir_all(&output_dir);
}

/// Task1260, on a real recording: point `LIVIA_TASK1260_CONTAINER` at a `.lvb`
/// and this reports what each of its segments actually carries -- how many
/// audio tracks, and how many samples landed on each.
///
/// Env-gated like `task088_dumps_audio_sample_duration_distribution_when_requested`
/// above: the evidence that matters for multi-track recording is a file the
/// machine produced, and there is no such file in CI.
#[test]
fn task1260_reports_audio_tracks_of_a_recorded_container_when_requested() {
    let Ok(path) = std::env::var("LIVIA_TASK1260_CONTAINER") else {
        return;
    };
    let mut reader = crate::ring_buffer::container::ContainerReader::open_for_reading(
        std::path::Path::new(&path),
    )
    .expect("the container opens");
    let snapshot = reader.snapshot().clone();
    println!(
        "task1260: {} segments, tracks={:?}",
        snapshot.segments.len(),
        snapshot
            .audio_tracks
            .iter()
            .map(|track| track.executable_name.clone())
            .collect::<Vec<_>>()
    );
    let indices: Vec<u64> = snapshot
        .segments
        .iter()
        .map(|segment| segment.index)
        .collect();
    let sampled: Vec<u64> = indices
        .iter()
        .copied()
        .enumerate()
        .filter(|(position, _)| {
            // First, middle and last: enough to see the shape without dumping
            // a whole recording.
            *position == 0 || *position == indices.len() / 2 || *position + 1 == indices.len()
        })
        .map(|(_, index)| index)
        .collect();
    let directory =
        std::env::temp_dir().join(format!("livia-task1260-read-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    for index in sampled {
        let bytes = reader.read_segment(index).expect("segment bytes");
        let file = directory.join(format!("segment-{index}.mp4"));
        std::fs::write(&file, &bytes).unwrap();
        let mut tracks = dump_segment_track_summary(&file).expect("track summary");
        tracks.sort_by_key(|track| track.track_id);
        let offsets = snapshot
            .segments
            .iter()
            .find(|segment| segment.index == index)
            .map(|segment| segment.audio_offsets_100ns.clone())
            .unwrap_or_default();
        println!("task1260: segment {index} offsets={offsets:?}");
        for track in &tracks {
            println!(
                "task1260:   track {} audio={} samples={}",
                track.track_id, track.is_audio, track.sample_count
            );
        }
        let audio: Vec<u64> = tracks
            .iter()
            .filter(|track| track.is_audio)
            .map(|track| track.sample_count)
            .collect();
        assert_eq!(
            audio.len(),
            offsets.len(),
            "segment {index} has {} audio tracks but {} offsets",
            audio.len(),
            offsets.len()
        );
        for (track, samples) in audio.iter().enumerate() {
            assert!(*samples > 0, "segment {index} audio track {track} is empty");
        }
    }
    let _ = std::fs::remove_dir_all(&directory);
}
