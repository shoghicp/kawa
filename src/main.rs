extern crate shout;
extern crate ffmpeg;
extern crate libc;

use std::env;

use libc::{c_int, uint8_t, c_void};
use ffmpeg::{format, codec, media, frame, filter};
use std::sync::{Arc, Mutex};

fn main() {
    ffmpeg::init().unwrap();
    match ffmpeg::format::input(&env::args().nth(1).expect("missing file")) {
        Ok(context) => {
            // let codec = ffmpeg::encoder::find(ffmpeg::codec::id::Id::VORBIS).unwrap().audio().unwrap();
            match transcode(context, "ogg", codec::id::Id::VORBIS) {
                Ok(f) => {
                    play(f);
                }
                Err(e) => {
                    println!("Error transcoding {}", e);
                }
            }
        }
        Err(error) => println!("error opening file: {}", error)
    }
}

struct Transcoder {
    stream:  usize,
    filter:  filter::Graph,
    decoder: codec::decoder::Audio,
    encoder: codec::encoder::Audio,
}

fn transcoder(ictx: &mut format::context::Input, octx: &mut format::context::Output, codec: codec::id::Id, filter_spec: &str) -> Result<Transcoder, ffmpeg::Error> {
    let input   = ictx.streams().best(media::Type::Audio).expect("could not find best audio stream");
    let decoder = try!(input.codec().decoder().audio());
    let codec = try!(ffmpeg::encoder::find(codec).unwrap().audio());
    let global  = octx.format().flags().contains(ffmpeg::format::flag::GLOBAL_HEADER);

    let mut output  = try!(octx.add_stream(codec));
    let mut encoder = try!(output.codec().encoder().audio());

    let channel_layout = codec.channel_layouts()
        .map(|cls| cls.best(decoder.channel_layout().channels()))
        .unwrap_or(ffmpeg::channel_layout::STEREO);

    if global {
        encoder.set_flags(ffmpeg::codec::flag::GLOBAL_HEADER);
    }

    encoder.set_rate(decoder.rate() as i32);
    encoder.set_channel_layout(channel_layout);
    encoder.set_channels(channel_layout.channels());
    encoder.set_format(codec.formats().expect("unknown supported formats").next().unwrap());
    encoder.set_bit_rate(decoder.bit_rate());
    encoder.set_max_bit_rate(decoder.max_bit_rate());

    encoder.set_time_base((1, decoder.rate() as i32));
    output.set_time_base((1, decoder.rate() as i32));

    let encoder = try!(encoder.open_as(codec));
    let filter  = try!(filter(filter_spec, &decoder, &encoder));

    Ok(Transcoder {
        stream:  input.index(),
        filter:  filter,
        decoder: decoder,
        encoder: encoder,
    })
}

macro_rules! rw_callback {
    ($name:ident, $func:ident, $t:ty) => {
        extern fn $name(opaque: *mut c_void, buffer: *mut uint8_t, buffer_len: c_int) -> c_int {
            unsafe {
                let output: &$t = &*(opaque as *const $t);
                let buffer = std::slice::from_raw_parts(buffer, buffer_len as usize);
                $func(output, buffer) as c_int
            }
        }
    }
}

fn write_to_buf(output: &Arc<Mutex<Vec<u8>>>, buffer: &[u8]) -> usize {
    use std::io::Write;
    let mut data = output.lock().unwrap();
    data.write(buffer).unwrap()
}

rw_callback!(write_packet, write_to_buf, Arc<Mutex<Vec<u8>>>);

fn transcode(mut ictx: format::context::Input, container: &str, format: codec::id::Id) -> Result<Vec<u8>, ffmpeg::Error> {
    let filter = "anull".to_owned();

    let buffer: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let io_ctx = format::io::Context::new(4096, true, buffer.clone(), None, Some(write_packet), None);
    let mut octx = format::open_custom_io(io_ctx, false, container).unwrap().output();
    let mut transcoder = transcoder(&mut ictx, &mut octx, format, &filter).unwrap();

    octx.set_metadata(ictx.metadata().to_owned());
    octx.write_header().unwrap();

    let in_time_base  = transcoder.decoder.time_base();
    let out_time_base = octx.stream(0).unwrap().time_base();

    let mut decoded = frame::Audio::empty();
    let mut encoded = ffmpeg::Packet::empty();
    for (stream, mut packet) in ictx.packets() {
        if stream.index() == transcoder.stream {
            packet.rescale_ts(stream.time_base(), in_time_base);

            if let Ok(true) = transcoder.decoder.decode(&packet, &mut decoded) {
                let timestamp = decoded.timestamp();
                decoded.set_pts(timestamp);

                transcoder.filter.get("in").unwrap().source().add(&decoded).unwrap();

                while let Ok(..) = transcoder.filter.get("out").unwrap().sink().frame(&mut decoded) {
                    if let Ok(true) = transcoder.encoder.encode(&decoded, &mut encoded) {
                        encoded.set_stream(0);
                        encoded.rescale_ts(in_time_base, out_time_base);
                        encoded.write_interleaved(&mut octx).unwrap();
                    }
                }
            }
        }
    }

    transcoder.filter.get("in").unwrap().source().flush().unwrap();

    while let Ok(..) = transcoder.filter.get("out").unwrap().sink().frame(&mut decoded) {
        if let Ok(true) = transcoder.encoder.encode(&decoded, &mut encoded) {
            encoded.set_stream(0);
            encoded.rescale_ts(in_time_base, out_time_base);
            encoded.write_interleaved(&mut octx).unwrap();
        }
    }

    if let Ok(true) = transcoder.encoder.flush(&mut encoded) {
        encoded.set_stream(0);
        encoded.rescale_ts(in_time_base, out_time_base);
        encoded.write_interleaved(&mut octx).unwrap();
    }

    octx.write_trailer().unwrap();
    drop(transcoder);
    drop(ictx);
    drop(octx);
    match Arc::try_unwrap(buffer) {
        Ok(d) => {
            println!("Data moved!");
            Ok(d.into_inner().unwrap())
        },
        Err(a) => {
            println!("Data copied!");
            let res = a.lock().unwrap().clone();
            Ok(res)
        }
    }
}

fn filter(spec: &str, decoder: &codec::decoder::Audio, encoder: &codec::encoder::Audio) -> Result<filter::Graph, ffmpeg::Error> {
    let mut filter = filter::Graph::new();

    let args = format!("time_base={}:sample_rate={}:sample_fmt={}:channel_layout=0x{:x}",
                       decoder.time_base(), decoder.rate(), decoder.format().name(), decoder.channel_layout().bits());

    try!(filter.add(&filter::find("abuffer").unwrap(), "in", &args));
    try!(filter.add(&filter::find("abuffersink").unwrap(), "out", ""));

    {
        let mut out = filter.get("out").unwrap();

        out.set_sample_format(encoder.format());
        out.set_channel_layout(encoder.channel_layout());
        out.set_sample_rate(encoder.rate());
    }

    try!(try!(try!(filter.output("in", 0)).input("out", 0)).parse(spec));
    try!(filter.validate());

    if let Some(codec) = encoder.codec() {
        if !codec.capabilities().contains(ffmpeg::codec::capabilities::VARIABLE_FRAME_SIZE) {
            filter.get("out").unwrap().sink().set_frame_size(encoder.frame_size());
        }
    }

    Ok(filter)
}


fn play(buffer: Vec<u8>) -> shout::ShoutConn {
    let conn = shout::ShoutConnBuilder::new()
        .host(String::from("radio.stew.moe"))
        .port(8000)
        .user(String::from("source"))
        .password(String::from("bDXsHJq9s2BvjWKeH5iO"))
        .mount(String::from("/test.ogg"))
        .protocol(shout::ShoutProtocol::HTTP)
        .format(shout::ShoutFormat::Ogg)
        .build()
        .unwrap();

    println!("Connected to server");
    let step = 4096;
    let mut pos = 0;
    loop {
        if pos + step < buffer.len() {
            conn.send(buffer[pos..(pos + step)].to_vec());
            pos += step;
            conn.sync();
        } else {
            conn.send(buffer[pos..(pos + (buffer.len() - pos))].to_vec());
            println!("Finished!");
            break;
        }
    }
    conn
}
