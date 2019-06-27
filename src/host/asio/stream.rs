extern crate asio_sys as sys;
extern crate num_traits;

use self::num_traits::PrimInt;
use super::asio_utils as au;
use super::Device;
use std;
use std::mem;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use BuildStreamError;
use Format;
use PauseStreamError;
use PlayStreamError;
use SampleFormat;
use StreamData;
use StreamDataResult;
use UnknownTypeInputBuffer;
use UnknownTypeOutputBuffer;

/// Controls all streams
pub struct EventLoop {
    /// The input and output ASIO streams
    asio_streams: Arc<Mutex<sys::AsioStreams>>,
    /// List of all CPAL streams
    cpal_streams: Arc<Mutex<Vec<Option<Stream>>>>,
    /// Total stream count
    stream_count: AtomicUsize,
    /// The CPAL callback that the user gives to fill the buffers.
    callbacks: Arc<Mutex<Option<&'static mut (FnMut(StreamId, StreamDataResult) + Send)>>>,
}

/// Id for each stream.
/// Created depending on the number they are created.
/// Starting at one! not zero.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StreamId(usize);

/// CPAL stream.
/// This decouples the many cpal streams
/// from the single input and single output
/// ASIO streams.
/// Each stream can be playing or paused.
struct Stream {
    playing: bool,
}

#[derive(Default)]
struct I16Buffer {
    cpal: Vec<i16>,
    channel: Vec<Vec<i16>>,
}

#[derive(Default)]
struct F32Buffer {
    cpal: Vec<f32>,
    channel: Vec<Vec<f32>>,
}
struct Buffers {
    i16_buff: I16Buffer,
    //u16_buff: U16Buffer,
    f32_buff: F32Buffer,
}

enum Endian {
    Little,
    Big,
}

impl EventLoop {
    pub fn new() -> EventLoop {
        EventLoop {
            asio_streams: Arc::new(Mutex::new(sys::AsioStreams {
                input: None,
                output: None,
            })),
            cpal_streams: Arc::new(Mutex::new(Vec::new())),
            // This is why the Id's count from one not zero
            // because at this point there is no streams
            stream_count: AtomicUsize::new(0),
            callbacks: Arc::new(Mutex::new(None)),
        }
    }

    fn check_format(
        &self,
        drivers: &sys::Drivers,
        format: &Format,
        num_asio_channels: u16,
    ) -> Result<(), BuildStreamError> {
        let Format {
            channels,
            sample_rate,
            data_type,
        } = format;
        // Try and set the sample rate to what the user selected.
        let sample_rate = sample_rate.0;
        if sample_rate != drivers.get_sample_rate().rate {
            if drivers.can_sample_rate(sample_rate) {
                drivers
                    .set_sample_rate(sample_rate)
                    .expect("Unsupported sample rate");
            } else {
                return Err(BuildStreamError::FormatNotSupported);
            }
        }
        // unsigned formats are not supported by asio
        match data_type {
            SampleFormat::I16 | SampleFormat::F32 => (),
            SampleFormat::U16 => return Err(BuildStreamError::FormatNotSupported),
        }
        if *channels > num_asio_channels {
            return Err(BuildStreamError::FormatNotSupported);
        }
        Ok(())
    }

    /// Create a new CPAL Input Stream.
    /// If there is no ASIO Input Stream
    /// it will be created.
    fn get_input_stream(
        &self,
        drivers: &sys::Drivers,
        format: &Format,
        device: &Device,
    ) -> Result<usize, BuildStreamError> {
        match device.default_input_format() {
            Ok(f) => {
                let num_asio_channels = f.channels;
                self.check_format(drivers, format, num_asio_channels)
            },
            Err(_) => Err(BuildStreamError::FormatNotSupported),
        }?;
        let num_channels = format.channels as usize;
        let ref mut streams = *self.asio_streams.lock().unwrap();
        // Either create a stream if thers none or had back the
        // size of the current one.
        match streams.input {
            Some(ref input) => Ok(input.buffer_size as usize),
            None => {
                let output = streams.output.take();
                drivers
                    .prepare_input_stream(output, num_channels)
                    .map(|new_streams| {
                        let bs = match new_streams.input {
                            Some(ref inp) => inp.buffer_size as usize,
                            None => unreachable!(),
                        };
                        *streams = new_streams;
                        bs
                    }).map_err(|ref e| {
                        println!("Error preparing stream: {}", e);
                        BuildStreamError::DeviceNotAvailable
                    })
            }
        }
    }

    /// Create a new CPAL Output Stream.
    /// If there is no ASIO Output Stream
    /// it will be created.
    fn get_output_stream(
        &self,
        drivers: &sys::Drivers,
        format: &Format,
        device: &Device,
    ) -> Result<usize, BuildStreamError> {
        match device.default_output_format() {
            Ok(f) => {
                let num_asio_channels = f.channels;
                self.check_format(drivers, format, num_asio_channels)
            },
            Err(_) => Err(BuildStreamError::FormatNotSupported),
        }?;
        let num_channels = format.channels as usize;
        let ref mut streams = *self.asio_streams.lock().unwrap();
        // Either create a stream if thers none or had back the
        // size of the current one.
        match streams.output {
            Some(ref output) => Ok(output.buffer_size as usize),
            None => {
                let input = streams.input.take();
                drivers
                    .prepare_output_stream(input, num_channels)
                    .map(|new_streams| {
                        let bs = match new_streams.output {
                            Some(ref out) => out.buffer_size as usize,
                            None => unreachable!(),
                        };
                        *streams = new_streams;
                        bs
                    }).map_err(|ref e| {
                        println!("Error preparing stream: {}", e);
                        BuildStreamError::DeviceNotAvailable
                    })
            }
        }
    }

    /// Builds a new cpal input stream
    pub fn build_input_stream(
        &self,
        device: &Device,
        format: &Format,
    ) -> Result<StreamId, BuildStreamError> {
        let Device { drivers, .. } = device;
        let num_channels = format.channels.clone();
        let stream_type = drivers.get_data_type().expect("Couldn't load data type");
        let input_stream = self.get_input_stream(&drivers, format, device);
        input_stream.map(|stream_buffer_size| {
            let cpal_num_samples = stream_buffer_size * num_channels as usize;
            let count = self.stream_count.fetch_add(1, Ordering::SeqCst);
            let asio_streams = self.asio_streams.clone();
            let cpal_streams = self.cpal_streams.clone();
            let callbacks = self.callbacks.clone();

            let channel_len = cpal_num_samples / num_channels as usize;

            // Create buffers depending on data type
            // TODO the naming of cpal and channel is confusing.
            // change it to:
            // cpal -> interleaved
            // channels -> per_channel
            let mut buffers = match format.data_type {
                SampleFormat::I16 => Buffers {
                    i16_buff: I16Buffer {
                        cpal: vec![0 as i16; cpal_num_samples],
                        channel: (0..num_channels)
                            .map(|_| Vec::with_capacity(channel_len))
                            .collect(),
                    },
                    f32_buff: F32Buffer::default(),
                },
                SampleFormat::F32 => Buffers {
                    i16_buff: I16Buffer::default(),
                    f32_buff: F32Buffer {
                        cpal: vec![0 as f32; cpal_num_samples],
                        channel: (0..num_channels)
                            .map(|_| Vec::with_capacity(channel_len))
                            .collect(),
                    },
                },
                _ => unimplemented!(),
            };

            // Set the input callback.
            // This is most performance critical part of the ASIO bindings.
            sys::set_callback(move |index| unsafe {
                // if not playing return early
                {
                    if let Some(s) = cpal_streams.lock().unwrap().get(count) {
                        if let Some(s) = s {
                            if !s.playing {
                                return ();
                            }
                        }
                    }
                }
                // Get the stream
                let stream_lock = asio_streams.lock().unwrap();
                let ref asio_stream = match stream_lock.input {
                    Some(ref asio_stream) => asio_stream,
                    None => return (),
                };

                // Get the callback
                let mut callbacks = callbacks.lock().unwrap();

                // Theres only a single callback because theres only one event loop
                let callback = match callbacks.as_mut() {
                    Some(callback) => callback,
                    None => return (),
                };

                // Macro to convert sample from ASIO to CPAL type
                macro_rules! convert_sample {
                    // floats types required different conversion
                    (f32,
                    f32,
                    $SampleTypeIdent:ident,
                    $Sample:expr
                    ) => {
                        *$Sample
                    };
                    (f64,
                    f64,
                    $SampleTypeIdent:ident,
                    $Sample:expr
                    ) => {
                        *$Sample
                    };
                    (f64,
                    f32,
                    $SampleTypeIdent:ident,
                    $Sample:expr
                    ) => {
                        *$Sample as f32
                    };
                    (f32,
                    f64,
                    $SampleTypeIdent:ident,
                    $Sample:expr
                    ) => {
                        *$Sample as f64
                    };
                    ($AsioTypeIdent:ident,
                    f32,
                    $SampleTypeIdent:ident,
                    $Sample:expr
                    ) => {
                        (*$Sample as f64 / ::std::$AsioTypeIdent::MAX as f64) as f32
                    };
                    ($AsioTypeIdent:ident,
                    f64,
                    $SampleTypeIdent:ident,
                    $Sample:expr
                    ) => {
                        *$Sample as f64 / ::std::$AsioTypeIdent::MAX as f64
                    };
                    (f32,
                    $SampleType:ty,
                    $SampleTypeIdent:ident,
                    $Sample:expr
                    ) => {
                        (*$Sample as f64 * ::std::$SampleTypeIdent::MAX as f64) as $SampleType
                    };
                    (f64,
                    $SampleType:ty,
                    $SampleTypeIdent:ident,
                    $Sample:expr
                    ) => {
                        (*$Sample as f64 * ::std::$SampleTypeIdent::MAX as f64) as $SampleType
                    };
                    ($AsioTypeIdent:ident,
                    $SampleType:ty,
                    $SampleTypeIdent:ident,
                    $Sample:expr
                    ) => {
                        (*$Sample as i64 * ::std::$SampleTypeIdent::MAX as i64
                            / ::std::$AsioTypeIdent::MAX as i64) as $SampleType
                    };
                };
                // This creates gets the buffer and interleaves it.
                // It allows it to be done based on the sample type.
                macro_rules! try_callback {
                    ($SampleFormat:ident,
                    $SampleType:ty,
                    $SampleTypeIdent:ident,
                    $AsioType:ty,
                    $AsioTypeIdent:ident,
                    $Buffers:expr,
                    $BuffersType:ty,
                    $BuffersTypeIdent:ident,
                    $Endianness:expr,
                    $ConvertEndian:expr
                    ) => {
                        // For each channel write the asio buffer to
                        // the cpal buffer

                        for (i, channel) in $Buffers.channel.iter_mut().enumerate() {
                            let buff_ptr = asio_stream.buffer_infos[i].buffers[index as usize]
                                as *mut $AsioType;
                            let asio_buffer: &'static [$AsioType] = std::slice::from_raw_parts(
                                buff_ptr,
                                asio_stream.buffer_size as usize,
                            );
                            for asio_s in asio_buffer.iter() {
                                channel.push($ConvertEndian(
                                    convert_sample!(
                                        $AsioTypeIdent,
                                        $SampleType,
                                        $SampleTypeIdent,
                                        asio_s
                                    ),
                                    $Endianness,
                                ));
                            }
                        }

                        // interleave all the channels
                        {
                            let $BuffersTypeIdent {
                                cpal: ref mut c_buffer,
                                channel: ref mut channels,
                            } = $Buffers;
                            au::interleave(&channels, c_buffer);
                            // Clear the per channel buffers
                            for c in channels.iter_mut() {
                                c.clear();
                            }
                        }

                        // Call the users callback with the buffer
                        callback(
                            StreamId(count),
                            Ok(StreamData::Input {
                                buffer: UnknownTypeInputBuffer::$SampleFormat(::InputBuffer {
                                    buffer: &$Buffers.cpal,
                                }),
                            }),
                        );
                    };
                };
                // Call the right buffer handler depending on types
                match stream_type {
                    sys::AsioSampleType::ASIOSTInt32LSB => {
                        try_callback!(
                            I16,
                            i16,
                            i16,
                            i32,
                            i32,
                            buffers.i16_buff,
                            I16Buffer,
                            I16Buffer,
                            Endian::Little,
                            convert_endian_from
                        );
                    }
                    sys::AsioSampleType::ASIOSTInt16LSB => {
                        try_callback!(
                            I16,
                            i16,
                            i16,
                            i16,
                            i16,
                            buffers.i16_buff,
                            I16Buffer,
                            I16Buffer,
                            Endian::Little,
                            convert_endian_from
                        );
                    }
                    sys::AsioSampleType::ASIOSTInt32MSB => {
                        try_callback!(
                            I16,
                            i16,
                            i16,
                            i32,
                            i32,
                            buffers.i16_buff,
                            I16Buffer,
                            I16Buffer,
                            Endian::Big,
                            convert_endian_from
                        );
                    }
                    sys::AsioSampleType::ASIOSTInt16MSB => {
                        try_callback!(
                            I16,
                            i16,
                            i16,
                            i16,
                            i16,
                            buffers.i16_buff,
                            I16Buffer,
                            I16Buffer,
                            Endian::Big,
                            convert_endian_from
                        );
                    }
                    sys::AsioSampleType::ASIOSTFloat32LSB => {
                        try_callback!(
                            F32,
                            f32,
                            f32,
                            f32,
                            f32,
                            buffers.f32_buff,
                            F32Buffer,
                            F32Buffer,
                            Endian::Little,
                            |a, _| a
                        );
                    }
                    sys::AsioSampleType::ASIOSTFloat64LSB => {
                        try_callback!(
                            F32,
                            f32,
                            f32,
                            f64,
                            f64,
                            buffers.f32_buff,
                            F32Buffer,
                            F32Buffer,
                            Endian::Little,
                            |a, _| a
                        );
                    }
                    sys::AsioSampleType::ASIOSTFloat32MSB => {
                        try_callback!(
                            F32,
                            f32,
                            f32,
                            f32,
                            f32,
                            buffers.f32_buff,
                            F32Buffer,
                            F32Buffer,
                            Endian::Big,
                            |a, _| a
                        );
                    }
                    sys::AsioSampleType::ASIOSTFloat64MSB => {
                        try_callback!(
                            F32,
                            f32,
                            f32,
                            f64,
                            f64,
                            buffers.f32_buff,
                            F32Buffer,
                            F32Buffer,
                            Endian::Big,
                            |a, _| a
                        );
                    }
                    _ => println!("unsupported format {:?}", stream_type),
                }
            });
            // Create stream and set to paused
            self.cpal_streams
                .lock()
                .unwrap()
                .push(Some(Stream { playing: false }));
            StreamId(count)
        })
    }

    /// Create the an output cpal stream.
    pub fn build_output_stream(
        &self,
        device: &Device,
        format: &Format,
    ) -> Result<StreamId, BuildStreamError> {
        let Device { drivers, .. } = device;
        let num_channels = format.channels.clone();
        let stream_type = drivers.get_data_type().expect("Couldn't load data type");
        let output_stream = self.get_output_stream(&drivers, format, device);
        output_stream.map(|stream_buffer_size| {
            let cpal_num_samples = stream_buffer_size * num_channels as usize;
            let count = self.stream_count.fetch_add(1, Ordering::SeqCst);
            let asio_streams = self.asio_streams.clone();
            let cpal_streams = self.cpal_streams.clone();
            let callbacks = self.callbacks.clone();
            let channel_len = cpal_num_samples / num_channels as usize;

            // Create buffers depending on data type
            let mut re_buffers = match format.data_type {
                SampleFormat::I16 => Buffers {
                    i16_buff: I16Buffer {
                        cpal: vec![0 as i16; cpal_num_samples],
                        channel: (0..num_channels)
                            .map(|_| Vec::with_capacity(channel_len))
                            .collect(),
                    },
                    f32_buff: F32Buffer::default(),
                },
                SampleFormat::F32 => Buffers {
                    i16_buff: I16Buffer::default(),
                    f32_buff: F32Buffer {
                        cpal: vec![0 as f32; cpal_num_samples],
                        channel: (0..num_channels)
                            .map(|_| Vec::with_capacity(channel_len))
                            .collect(),
                    },
                },
                _ => unimplemented!(),
            };

            sys::set_callback(move |index| unsafe {
                // if not playing return early
                {
                    if let Some(s) = cpal_streams.lock().unwrap().get(count) {
                        if let Some(s) = s {
                            if !s.playing {
                                return ();
                            }
                        }
                    }
                }
                // Get the stream
                let stream_lock = asio_streams.lock().unwrap();
                let ref asio_stream = match stream_lock.output {
                    Some(ref asio_stream) => asio_stream,
                    None => return (),
                };

                // Get the callback
                let mut callbacks = callbacks.lock().unwrap();

                // Theres only a single callback because theres only one event loop
                let callback = match callbacks.as_mut() {
                    Some(callback) => callback,
                    None => return (),
                };

                // Convert sample depending on the sample type
                macro_rules! convert_sample {
                    ($AsioTypeIdent:ident,
                    f64,
                    f64,
                    $Sample:expr
                    ) => {
                        *$Sample
                    };
                    ($AsioTypeIdent:ident,
                    f32,
                    f32,
                    $Sample:expr
                    ) => {
                        *$Sample
                    };
                    ($AsioTypeIdent:ident,
                    f64,
                    f32,
                    $Sample:expr
                    ) => {
                        *$Sample as f64
                    };
                    ($AsioTypeIdent:ident,
                    f32,
                    f64,
                    $Sample:expr
                    ) => {
                        *$Sample as f32
                    };
                    ($AsioTypeIdent:ident,
                    $AsioType:ty,
                    f32,
                    $Sample:expr
                    ) => {
                        (*$Sample as f64 * ::std::$AsioTypeIdent::MAX as f64) as $AsioType
                    };
                    ($AsioTypeIdent:ident,
                    $AsioType:ty,
                    f64,
                    $Sample:expr
                    ) => {
                        (*$Sample as f64 * ::std::$AsioTypeIdent::MAX as f64) as $AsioType
                    };
                    ($AsioTypeIdent:ident,
                    f32,
                    $SampleTypeIdent:ident,
                    $Sample:expr
                    ) => {
                        (*$Sample as f64 / ::std::$SampleTypeIdent::MAX as f64) as f32
                    };
                    ($AsioTypeIdent:ident,
                    f64,
                    $SampleTypeIdent:ident,
                    $Sample:expr
                    ) => {
                        *$Sample as f64 / ::std::$SampleTypeIdent::MAX as f64
                    };
                    ($AsioTypeIdent:ident,
                    $AsioType:ty,
                    $SampleTypeIdent:ident,
                    $Sample:expr
                    ) => {
                        (*$Sample as i64 * ::std::$AsioTypeIdent::MAX as i64
                            / ::std::$SampleTypeIdent::MAX as i64) as $AsioType
                    };
                };

                macro_rules! try_callback {
                    ($SampleFormat:ident,
                    $SampleType:ty,
                    $SampleTypeIdent:ident,
                    $AsioType:ty,
                    $AsioTypeIdent:ident,
                    $Buffers:expr,
                    $BuffersType:ty,
                    $BuffersTypeIdent:ident,
                    $Endianness:expr,
                    $ConvertEndian:expr
                    ) => {
                        let mut my_buffers = $Buffers;
                        {
                            // call the callback to fill the buffer with
                            // users data
                            callback(
                                StreamId(count),
                                Ok(StreamData::Output {
                                    buffer: UnknownTypeOutputBuffer::$SampleFormat(
                                        ::OutputBuffer {
                                            buffer: &mut my_buffers.cpal,
                                        },
                                    ),
                                }),
                            );
                        }
                        // Deinter all the channels
                        {
                            let $BuffersTypeIdent {
                                cpal: ref mut c_buffer,
                                channel: ref mut channels,
                            } = my_buffers;
                            au::deinterleave(&c_buffer[..], channels);
                        }

                        // Silence the buffer that is about to be used.
                        // This checks if any other callbacks have already
                        // silenced this buffer. If not it will silence it
                        // and set the opposite buffer half to unsilenced.
                        let silence = match index {
                            0 => {
                                if !sys::SILENCE_FIRST.load(Ordering::SeqCst) {
                                    sys::SILENCE_FIRST.store(true, Ordering::SeqCst);
                                    sys::SILENCE_SECOND.store(false, Ordering::SeqCst);
                                    true
                                } else {
                                    false
                                }
                            }
                            1 => {
                                if !sys::SILENCE_SECOND.load(Ordering::SeqCst) {
                                    sys::SILENCE_SECOND.store(true, Ordering::SeqCst);
                                    sys::SILENCE_FIRST.store(false, Ordering::SeqCst);
                                    true
                                } else {
                                    false
                                }
                            }
                            _ => unreachable!(),
                        };

                        // For each channel write the cpal data to
                        // the asio buffer
                        for (i, channel) in my_buffers.channel.iter().enumerate() {
                            let buff_ptr = asio_stream.buffer_infos[i].buffers
                                [index as usize] as *mut $AsioType;
                            let asio_buffer: &'static mut [$AsioType] =
                                std::slice::from_raw_parts_mut(
                                    buff_ptr,
                                    asio_stream.buffer_size as usize,
                                );
                            for (asio_s, cpal_s) in asio_buffer.iter_mut().zip(channel) {
                                if silence {
                                    *asio_s = 0.0 as $AsioType;
                                }
                                *asio_s += $ConvertEndian(
                                    convert_sample!(
                                        $AsioTypeIdent,
                                        $AsioType,
                                        $SampleTypeIdent,
                                        cpal_s
                                    ),
                                    $Endianness,
                                );
                            }
                        }
                    };
                }
                // Choose the buffer conversions based on the sample types
                match stream_type {
                    sys::AsioSampleType::ASIOSTInt32LSB => {
                        try_callback!(
                            I16,
                            i16,
                            i16,
                            i32,
                            i32,
                            &mut re_buffers.i16_buff,
                            I16Buffer,
                            I16Buffer,
                            Endian::Little,
                            convert_endian_to
                        );
                    }
                    sys::AsioSampleType::ASIOSTInt16LSB => {
                        try_callback!(
                            I16,
                            i16,
                            i16,
                            i16,
                            i16,
                            &mut re_buffers.i16_buff,
                            I16Buffer,
                            I16Buffer,
                            Endian::Little,
                            convert_endian_to
                        );
                    }
                    sys::AsioSampleType::ASIOSTInt32MSB => {
                        try_callback!(
                            I16,
                            i16,
                            i16,
                            i32,
                            i32,
                            &mut re_buffers.i16_buff,
                            I16Buffer,
                            I16Buffer,
                            Endian::Big,
                            convert_endian_to
                        );
                    }
                    sys::AsioSampleType::ASIOSTInt16MSB => {
                        try_callback!(
                            I16,
                            i16,
                            i16,
                            i16,
                            i16,
                            &mut re_buffers.i16_buff,
                            I16Buffer,
                            I16Buffer,
                            Endian::Big,
                            convert_endian_to
                        );
                    }
                    sys::AsioSampleType::ASIOSTFloat32LSB => {
                        try_callback!(
                            F32,
                            f32,
                            f32,
                            f32,
                            f32,
                            &mut re_buffers.f32_buff,
                            F32Buffer,
                            F32Buffer,
                            Endian::Little,
                            |a, _| a
                        );
                    }
                    sys::AsioSampleType::ASIOSTFloat64LSB => {
                        try_callback!(
                            F32,
                            f32,
                            f32,
                            f64,
                            f64,
                            &mut re_buffers.f32_buff,
                            F32Buffer,
                            F32Buffer,
                            Endian::Little,
                            |a, _| a
                        );
                    }
                    sys::AsioSampleType::ASIOSTFloat32MSB => {
                        try_callback!(
                            F32,
                            f32,
                            f32,
                            f32,
                            f32,
                            &mut re_buffers.f32_buff,
                            F32Buffer,
                            F32Buffer,
                            Endian::Big,
                            |a, _| a
                        );
                    }
                    sys::AsioSampleType::ASIOSTFloat64MSB => {
                        try_callback!(
                            F32,
                            f32,
                            f32,
                            f64,
                            f64,
                            &mut re_buffers.f32_buff,
                            F32Buffer,
                            F32Buffer,
                            Endian::Big,
                            |a, _| a
                        );
                    }
                    _ => println!("unsupported format {:?}", stream_type),
                }
            });
            // Create the stream paused
            self.cpal_streams
                .lock()
                .unwrap()
                .push(Some(Stream { playing: false }));
            // Give the ID based on the stream count
            StreamId(count)
        })
    }

    /// Play the cpal stream for the given ID.
    /// Also play The ASIO streams if they are not already.
    pub fn play_stream(&self, stream_id: StreamId) -> Result<(), PlayStreamError> {
        let mut streams = self.cpal_streams.lock().unwrap();
        if let Some(s) = streams.get_mut(stream_id.0).expect("Bad play stream index") {
            s.playing = true;
        }
        // Calling play when already playing is a no-op
        sys::play();
        Ok(())
    }

    /// Pause the cpal stream for the given ID.
    /// Pause the ASIO streams if there are no CPAL streams palying.
    pub fn pause_stream(&self, stream_id: StreamId) -> Result<(), PauseStreamError> {
        let mut streams = self.cpal_streams.lock().unwrap();
        if let Some(s) = streams
            .get_mut(stream_id.0)
            .expect("Bad pause stream index")
        {
            s.playing = false;
        }
        let any_playing = streams
            .iter()
            .any(|s| if let Some(s) = s { s.playing } else { false });
        if any_playing {
            sys::stop();
        }
        Ok(())
    }

    /// Destroy the cpal stream based on the ID.
    pub fn destroy_stream(&self, stream_id: StreamId) {
        let mut streams = self.cpal_streams.lock().unwrap();
        streams.get_mut(stream_id.0).take();
    }

    /// Run the cpal callbacks
    pub fn run<F>(&self, mut callback: F) -> !
    where
        F: FnMut(StreamId, StreamDataResult) + Send,
    {
        let callback: &mut (FnMut(StreamId, StreamDataResult) + Send) = &mut callback;
        // Transmute needed to convince the compiler that the callback has a static lifetime
        *self.callbacks.lock().unwrap() = Some(unsafe { mem::transmute(callback) });
        loop {
            // A sleep here to prevent the loop being
            // removed in --release
            thread::sleep(Duration::new(1u64, 0u32));
        }
    }
}

/// Clean up if event loop is dropped.
/// Currently event loop is never dropped.
impl Drop for EventLoop {
    fn drop(&mut self) {
        *self.asio_streams.lock().unwrap() = sys::AsioStreams {
            output: None,
            input: None,
        };
        sys::clean_up();
    }
}

/// Helper function to convert to system endianness
fn convert_endian_to<T: PrimInt>(sample: T, endian: Endian) -> T {
    match endian {
        Endian::Big => sample.to_be(),
        Endian::Little => sample.to_le(),
    }
}

/// Helper function to convert from system endianness
fn convert_endian_from<T: PrimInt>(sample: T, endian: Endian) -> T {
    match endian {
        Endian::Big => T::from_be(sample),
        Endian::Little => T::from_le(sample),
    }
}
