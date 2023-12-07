use std::collections::VecDeque;
use std::thread::{spawn, JoinHandle};
use std::sync::mpsc::{channel, Sender, Receiver};
use std::time::Duration;

use num_enum::{FromPrimitive, IntoPrimitive};
use rusb::{
    Context,
    Device,
    DeviceHandle,
    UsbContext,
    Version
};

const VID: u16 = 0x1d50;
const PID: u16 = 0x615b;

const MIN_SUPPORTED: Version = Version(0, 0, 4);
const NOT_SUPPORTED: Version = Version(0, 0, 5);

const ENDPOINT: u8 = 0x81;

const READ_LEN: usize = 0x4000;

#[derive(Copy, Clone, FromPrimitive, IntoPrimitive)]
#[repr(u8)]
pub enum Speed {
    #[default]
    High = 0,
    Full = 1,
    Low  = 2,
    Auto = 3,
}

impl Speed {
    pub fn description(&self) -> &'static str {
        use Speed::*;
        match self {
            Auto => "Auto",
            High => "High (480Mbps)",
            Full => "Full (12Mbps)",
            Low => "Low (1.5Mbps)",
        }
    }

    pub fn mask(&self) -> u8 {
        use Speed::*;
        match self {
            Auto => 0b0001,
            Low  => 0b0010,
            Full => 0b0100,
            High => 0b1000,
        }
    }
}

bitfield! {
    #[derive(Copy, Clone)]
    struct State(u8);
    bool, enable, set_enable: 0;
    u8, from into Speed, speed, set_speed: 2, 1;
}

impl State {
    fn new(enable: bool, speed: Speed) -> State {
        let mut state = State(0);
        state.set_enable(enable);
        state.set_speed(speed);
        state
    }
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error(transparent)]
    Usb(#[from] rusb::Error),
    #[error("channel send error")]
    ChannelSend,
    #[error("worker thread panic")]
    ThreadPanic,
    #[error("unsupported analyzer version: Gateware version is {0}. \
             Supported range is {MIN_SUPPORTED} or higher, \
             but not {NOT_SUPPORTED} or higher")]
    WrongVersion(Version),
}

/// A Luna device attached to the system.
pub struct LunaDevice {
    usb_device: Device<Context>,
    pub description: String,
    pub speeds: Vec<Speed>,
}

/// A handle to an open Luna device.
pub struct LunaHandle {
    usb_handle: DeviceHandle<Context>,
}

pub struct LunaStream {
    receiver: Receiver<Result<Packet, Error>>,
}

pub struct LunaStop {
    stop_request: Sender<()>,
    worker: JoinHandle::<Result<(), Error>>,
}

impl LunaDevice {
    pub fn scan(context: &mut Context) -> Result<Vec<LunaDevice>, Error> {
        let devices = context.devices()?;
        let mut result = Vec::with_capacity(devices.len());
        for usb_device in devices.iter() {
            let desc = usb_device.device_descriptor()?;
            if desc.vendor_id() == VID && desc.product_id() == PID {
                let handle = LunaHandle::new(usb_device.open()?)?;
                let description = handle.description()?;
                let speeds = handle.speeds()?;
                result.push(LunaDevice{
                    usb_device,
                    description,
                    speeds,
                })
            }
        }
        Ok(result)
    }

    pub fn open(&self) -> Result<LunaHandle, Error> {
        LunaHandle::new(self.usb_device.open()?)
    }
}

impl LunaHandle {
    fn new(usb_handle: DeviceHandle<Context>) -> Result<Self, Error> {
        let version = usb_handle
            .device()
            .device_descriptor()
            .map_err(Error::Usb)?
            .device_version();
        if version >= MIN_SUPPORTED && version < NOT_SUPPORTED {
            Ok(Self { usb_handle })
        } else {
            Err(Error::WrongVersion(version))
        }
    }

    pub fn description(&self) -> Result<String, Error> {
        let desc = self.usb_handle.device().device_descriptor()?;
        let manufacturer = self.usb_handle.read_manufacturer_string_ascii(&desc)?;
        let product = self.usb_handle.read_product_string_ascii(&desc)?;
        Ok(format!("{} {}", manufacturer, product))
    }

    pub fn speeds(&self) -> Result<Vec<Speed>, Error> {
        use rusb::{Direction, RequestType, Recipient, request_type};
        let mut buf = [0u8];
        self.usb_handle.read_control(
            request_type(Direction::In, RequestType::Vendor, Recipient::Device),
            2,
            0,
            0,
            &mut buf,
            Duration::from_secs(5),
        )?;
        let mut speeds = vec![];
        use Speed::*;
        for speed in [Auto, High, Full, Low] {
            if buf[0] & speed.mask() != 0 {
                speeds.push(speed);
            }
        }
        Ok(speeds)
    }

    pub fn start(mut self, speed: Speed)
        -> Result<(LunaStream, LunaStop), Error>
    {
        self.usb_handle.claim_interface(0)?;
        let (tx, rx) = channel();
        let (stop_tx, stop_rx) = channel();
        let worker = spawn(move || {
            let mut buffer = [0u8; READ_LEN];
            let mut packet_queue = PacketQueue::new();
            let mut state = State::new(true, speed);
            self.write_state(state)?;
            println!("Capture enabled, speed: {}", speed.description());
            while stop_rx.try_recv().is_err() {
                let result = self.usb_handle.read_bulk(
                    ENDPOINT, &mut buffer, Duration::from_millis(100));
                match result {
                    Ok(count) => {
                        packet_queue.extend(&buffer[..count]);
                        while let Some(packet) = packet_queue.next() {
                            tx.send(Ok(packet))
                                .or(Err(Error::ChannelSend))?;
                        };
                    },
                    Err(rusb::Error::Timeout) => continue,
                    Err(usb_error) => {
                        tx.send(Err(Error::from(usb_error)))
                            .or(Err(Error::ChannelSend))?;
                        return Err(Error::from(usb_error));
                    }
                }
            }
            state.set_enable(false);
            self.write_state(state)?;
            println!("Capture disabled");
            Ok(())
        });
        Ok((
            LunaStream {
                receiver: rx,
            },
            LunaStop {
                stop_request: stop_tx,
                worker,
            }
        ))
    }

    fn write_state(&mut self, state: State) -> Result<(), Error> {
        use rusb::{Direction, RequestType, Recipient, request_type};
        self.usb_handle.write_control(
            request_type(Direction::Out, RequestType::Vendor, Recipient::Device),
            1,
            u16::from(state.0),
            0,
            &[],
            Duration::from_secs(5),
        )?;
        Ok(())
    }
}

impl LunaStream {
    pub fn next(&mut self) -> Option<Result<Packet, Error>> {
        self.receiver.recv().ok()
    }
}

impl LunaStop {
    pub fn stop(self) -> Result<(), Error> {
        use Error::*;
        println!("Requesting capture stop");
        self.stop_request.send(()).or(Err(ChannelSend))?;
        self.worker.join().or(Err(ThreadPanic))?
    }
}

pub struct Packet {
    pub timestamp: u64,
    pub bytes: Vec<u8>,
}

struct PacketQueue {
    buffer: VecDeque<u8>,
    total_cycles: u64,
}

impl PacketQueue {
    pub fn new() -> Self {
        PacketQueue {
            buffer: VecDeque::new(),
            total_cycles: 0,
        }
    }

    pub fn extend(&mut self, slice: &[u8]) {
        self.buffer.extend(slice.iter());
    }

    pub fn next(&mut self) -> Option<Packet> {
        let buffer_len = self.buffer.len();
        if buffer_len <= 2 {
            return None;
        }
        let packet_len = u16::from_be_bytes([self.buffer[0], self.buffer[1]]) as usize;
        if buffer_len <= 4 + packet_len + (packet_len % 2) {
            return None;
        }

        let clk_cycles = u16::from_be_bytes([self.buffer[2], self.buffer[3]]);

        self.buffer.drain(0..4);

        let bytes = self.buffer.drain(0..packet_len).collect();

        if packet_len % 2 != 0 {
            self.buffer.drain(0..1);
        }

        self.total_cycles += clk_cycles as u64;

        let timestamp = clk_to_ns(self.total_cycles);

        Some(Packet{timestamp, bytes})
    }
}

fn clk_to_ns(clk_cycles: u64) -> u64 {
    const TABLE: [u64; 3] = [0, 16, 33];
    let quotient = clk_cycles / 3;
    let remainder = clk_cycles % 3;
    return quotient * 50 + TABLE[remainder as usize];
}
