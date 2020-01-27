//use std::result::Result;
//use std::io::{ErrorKind};
use chrono::prelude::*;
use crc::{crc16, Hasher16};
use std::io;
use std::time::{Duration, Instant};
use crate::error::{Error, Result};

pub use crate::ubx_packets::*;
pub use crate::segmenter::Segmenter;

mod error;
mod ubx_packets;
mod segmenter;

#[derive(Debug)]
pub enum ResetType {
    Hot,
    Warm,
    Cold,
}

pub struct Device {
    port: Box<dyn serialport::SerialPort>,
    segmenter: Segmenter,
    //buf: Vec<u8>,

    alp_data: Vec<u8>,
    alp_file_id: u16,

    navpos: Option<NavPosLLH>,
    navvel: Option<NavVelNED>,
    navstatus: Option<NavStatus>,
    solution: Option<NavPosVelTime>,
}

impl Device {
    pub fn new() -> Result<Device> {
        let s = serialport::SerialPortSettings {
            baud_rate: 9600,
            data_bits: serialport::DataBits::Eight,
            flow_control: serialport::FlowControl::None,
            parity: serialport::Parity::None,
            stop_bits: serialport::StopBits::One,
            timeout: Duration::from_millis(500),
        };
        let port = serialport::open_with_settings("/dev/ttyUSB0", &s).unwrap();
        let mut dev = Device {
            port: port,
            segmenter: Segmenter::new(),
            //buf: Vec::new(),
            alp_data: Vec::new(),
            alp_file_id: 0,
            navpos: None,
            navvel: None,
            navstatus: None,
            solution: None,
        };

        dev.init_protocol()?;
        Ok(dev)
    }

    fn init_protocol(&mut self) -> Result<()> {
        // Disable NMEA output in favor of the UBX protocol
        self.send(
            CfgPrtUart {
                portid: 1,
                reserved0: 0,
                tx_ready: 0,
                mode: 0x8d0,
                baud_rate: 9600,
                in_proto_mask: 0x07,
                out_proto_mask: 0x01,
                flags: 0,
                reserved5: 0,
            }
            .into(),
        )?;

        // Eat the acknowledge and let the device start
        self.wait_for_ack(0x06, 0x00)?;

        self.enable_packet(0x01, 0x07)?; // Nav pos vel time
        //self.enable_packet(0x01, 0x02)?; // Nav pos
        //self.enable_packet(0x01, 0x03)?; // Nav status
        //self.enable_packet(0x01, 0x12)?; // Nav velocity NED

        // Go get mon-ver
        self.send(UbxPacket {
            class: 0x0A,
            id: 0x04,
            payload: vec![],
        })?;
        self.poll_for(Duration::from_millis(200))?;

        Ok(())
    }

    fn enable_packet(&mut self, classid: u8, msgid: u8) -> Result<()> {
        self.send(
            CfgMsg {
                classid: classid,
                msgid: msgid,
                rates: [0, 1, 0, 0, 0, 0],
            }
            .into(),
        )?;
        self.wait_for_ack(0x06, 0x01)?;
        Ok(())
    }

    fn wait_for_ack(&mut self, classid: u8, msgid: u8) -> Result<()> {
        let now = Instant::now();
        while now.elapsed() < Duration::from_millis(1_000) {
            match self.get_next_message()? {
                Some(Packet::AckAck(packet)) => {
                    if packet.classid != classid || packet.msgid != msgid {
                        panic!("Expecting ack, got ack for wrong packet!");
                    }
                    return Ok(());
                }
                Some(_) => {
                    return Err(Error::UnexpectedPacket);
                }
                None => {
                    // Keep waiting
                }
            }
        }
        return Err(Error::TimedOutWaitingForAck(classid, msgid));
    }

    pub fn poll_for(&mut self, duration: Duration) -> Result<()> {
        let start = Instant::now();
        while start.elapsed() < duration {
            self.poll()?;
        }
        Ok(())
    }

    pub fn poll(&mut self) -> Result<()> {
        self.get_next_message()?;
        Ok(())
    }

    pub fn get_position(&mut self) -> Option<Position> {
        match (&self.navstatus, &self.navpos) {
            (Some(status), Some(pos)) => {
                if status.itow != pos.itow {
                    None
                } else if status.flags & 0x1 == 0 {
                    None
                } else {
                    Some(pos.into())
                }
            }
            _ => None,
        }
    }

    pub fn get_velocity(&mut self) -> Option<Velocity> {
        match (&self.navstatus, &self.navvel) {
            (Some(status), Some(vel)) => {
                if status.itow != vel.itow {
                    None
                } else if status.flags & 0x1 == 0 {
                    None
                } else {
                    Some(vel.into())
                }
            }
            _ => None,
        }
    }

    pub fn get_solution(&mut self) -> (Option<Position>, Option<Velocity>, Option<DateTime<Utc>>) {
        match &self.solution {
            Some(sol) => {
                let has_time = sol.fix_type == 0x03 || sol.fix_type == 0x04 || sol.fix_type == 0x05;
                let has_posvel = sol.fix_type == 0x03 || sol.fix_type == 0x04;
                let pos = if has_posvel { Some(sol.into()) } else { None };

                let vel = if has_posvel { Some(sol.into()) } else { None };

                let time = if has_time { Some(sol.into()) } else { None };
                (pos, vel, time)
            }
            None => (None, None, None),
        }
    }

    pub fn reset(&mut self, temperature: &ResetType) -> Result<()> {
        match temperature {
            ResetType::Hot => {
                self.send(CfgRst::HOT.into())?;
            }
            ResetType::Warm => {
                self.send(CfgRst::WARM.into())?;
            }
            ResetType::Cold => {
                self.send(CfgRst::COLD.into())?;
            }
        }

        // Clear our device state
        self.navpos = None;
        self.navstatus = None;

        // Wait a bit for it to reset
        // (we can't wait for the ack, because we get a bad checksum)
        let now = Instant::now();
        while now.elapsed() < Duration::from_millis(500) {
            //self.poll();
            // Eat any messages
            self.recv()?;
        }

        self.init_protocol()?;
        Ok(())
    }

    pub fn load_aid_data(
        &mut self,
        position: Option<Position>,
        tm: Option<DateTime<Utc>>,
    ) -> Result<()> {
        let mut aid = AidIni::new();
        match position {
            Some(pos) => {
                aid.set_position(pos);
            }
            _ => {}
        };
        match tm {
            Some(tm) => {
                aid.set_time(tm);
            }
            _ => {}
        };

        self.send(UbxPacket {
            class: 0x0B,
            id: 0x01,
            payload: bincode::serialize(&aid).unwrap(),
        })?;
        Ok(())
    }

    pub fn set_alp_offline(&mut self, data: &[u8]) -> Result<()> {
        self.alp_data = vec![0; data.len()];
        self.alp_data.copy_from_slice(data);

        let mut digest = crc16::Digest::new(crc16::X25);
        digest.write(&self.alp_data);
        self.alp_file_id = digest.sum16();

        self.send(UbxPacket {
            class: 0x06,
            id: 0x01,
            payload: vec![0x0B, 0x32, 0x01],
        })?;
        self.wait_for_ack(0x06, 0x01)?;
        Ok(())
    }

    fn get_next_message(&mut self) -> Result<Option<Packet>> {
        let packet = self.recv()?;
        match packet {
            Some(Packet::AckAck(packet)) => {
                //let packet: AckAck = bincode::deserialize(&packet.payload).unwrap();
                return Ok(Some(Packet::AckAck(packet)));
            }
            Some(Packet::MonVer(packet)) => {
                println!("Got versions: SW={} HW={}", packet.sw_version, packet.hw_version);
                return Ok(None);
            }
            Some(Packet::NavPosVelTime(packet)) => {
                self.solution = Some(packet);
                return Ok(None);
            }
            Some(Packet::NavVelNED(packet)) => {
                self.navvel = Some(packet);
                return Ok(None);
            }
            Some(Packet::NavStatus(packet)) => {
                self.navstatus = Some(packet);
                return Ok(None);
            }
            Some(Packet::NavPosLLH(packet)) => {
                self.navpos = Some(packet);
                return Ok(None);
            }
            Some(Packet::AlpSrv(packet)) => {
                if self.alp_data.len() == 0 {
                    // Uh-oh... we must be connecting to a device which was already in alp mode, let's just ignore it
                    return Ok(None);
                }

                let offset = packet.offset as usize * 2;
                let mut size = packet.size as usize * 2;
                println!(
                    "Got ALP request for contents offset={} size={}",
                    offset, size
                );

                let mut reply = packet.clone();
                reply.file_id = self.alp_file_id;

                if offset > self.alp_data.len() {
                    size = 0;
                } else if offset + size > self.alp_data.len() {
                    size = self.alp_data.len() - reply.offset as usize;
                }
                reply.data_size = size as u16;

                //println!("Have {} bytes of data, ultimately requesting range {}..{}", self.alp_data.len(), offset, offset+size);
                let contents = &self.alp_data[offset..offset + size];
                let mut payload = bincode::serialize(&reply).unwrap();
                for b in contents.iter() {
                    payload.push(*b);
                }
                //println!("Payload size: {}", payload.len());
                self.send(UbxPacket {
                    class: 0x0B,
                    id: 0x32,
                    payload: payload,
                })?;

                return Ok(None);
            }
            Some(packet) => {
                println!("Received packet {:?}", packet);
                return Ok(None);
            }
            None => {
                // Got nothing, do nothing
                return Ok(None);
            }
        }
    }

    pub fn send(&mut self, packet: UbxPacket) -> Result<()> {
        let serialized = packet.serialize();
        //println!("About to try sending {} bytes", serialized.len());
        self.port.write_all(&serialized)?;
        //println!("{} bytes successfully written, of {}", bytes_written, serialized.len());
        Ok(())
    }

    pub fn recv(&mut self) -> Result<Option<Packet>> {
        // Read bytes until we see the header 0xB5 0x62
        loop {
            let mut local_buf = [0; 1];
            let bytes_read = match self.port.read(&mut local_buf) {
                Ok(b) => b,
                Err(e) => {
                    if e.kind() == io::ErrorKind::TimedOut {
                        return Ok(None);
                    } else {
                        return Err(Error::IoError(e));
                    }
                }
            };

            if bytes_read == 0 {
                return Ok(None);
            }

            match self.segmenter.consume(&local_buf[..bytes_read])? {
                Some(packet) => {
                    return Ok(Some(packet));
                }
                None => {
                    // Do nothing
                }
            }
        }
    }
}
