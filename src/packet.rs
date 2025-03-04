#![allow(unused)]
use anyhow::{Context, Result};
use brotli::Decompressor;
use bytes::{BufMut, BytesMut};
use flate2::bufread::ZlibDecoder;
use serde_json::json;
use std::io::Read;

#[repr(u16)]
pub enum Version {
    Normal = 0,
    HeartbeatAuth = 1,
    Deflate = 2,
    Brotli = 3,
}

#[repr(u32)]
pub enum OpCode {
    Handshake = 0,
    HandshakeReply = 1,
    Heartbeat = 2,
    HeartbeatReply = 3,
    SendMsg = 4,
    SendMsgReply = 5,
    DisconnectReply = 6,
    Auth = 7,
    AuthReply = 8,
    Raw = 9,
    ProtoReady = 10,
    ProtoFinish = 11,
    ChangeRoom = 12,
    ChangeRoomReply = 13,
    Register = 14,
    RegisterReply = 15,
    Unregister = 16,
    UnregisterReply = 17,
}

const HEADER_MIN_SIZE: usize = 16;
const DEFAULT_SEQUENCE: u32 = 1;

#[derive(Debug)]
pub struct Packet {
    // 包总长度4字节
    pub total_size: u32,

    // 头部长度2字节
    pub header_size: u16,

    // 协议版本2字节：
    // 0: 普通包 (正文不使用压缩)
    // 1: 心跳及认证包 (正文不使用压缩)
    // 2: 普通包 (正文使用 zlib 压缩)
    // 3: 普通包 (使用 brotli 压缩的多个带文件头的普通包)
    pub version: u16,

    // 操作码4字节：
    // 2：心跳包
    // 3：心跳包回复 (人气值)
    // 5：普通包 (命令)
    // 7：认证包
    // 8：认证包回复
    pub opcode: u32,

    // 序列号4字节，默认1
    pub sequence: u32,

    // 包体 = 包总长度-头部长度
    pub body: Vec<u8>,
}

impl Packet {
    pub fn to_string(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }

    pub fn to_json(&self) -> Result<serde_json::Value> {
        serde_json::from_slice(&self.body).context("JSON 解析失败")
    }
}

trait FromBeBytes: Sized {
    fn from_be_bytes_slice(bytes: &[u8]) -> Result<Self>;
}

macro_rules! impl_from_be_bytes {
    ($($t:ty),+) => {
        $(
            impl FromBeBytes for $t {
                fn from_be_bytes_slice(bytes: &[u8]) -> Result<Self> {
                    let size = std::mem::size_of::<$t>();
                    if bytes.len() < size {
                        return Err(anyhow::anyhow!("数据长度不足"));
                    }
                    let arr = bytes[..size]
                        .try_into()
                        .map_err(|_| anyhow::anyhow!("读取字节失败"))?;
                    Ok(<$t>::from_be_bytes(arr))
                }
            }
        )+
    };
}

impl_from_be_bytes!(u16, u32);

fn parse_header(buf: &[u8]) -> Result<(u32, u16, u16, u32, u32)> {
    if buf.len() < HEADER_MIN_SIZE {
        return Err(anyhow::anyhow!("数据长度不足"));
    }

    let total_size = u32::from_be_bytes_slice(&buf[0..4])?;
    let header_size = u16::from_be_bytes_slice(&buf[4..6])?;
    let version = u16::from_be_bytes_slice(&buf[6..8])?;
    let opcode = u32::from_be_bytes_slice(&buf[8..12])?;
    let sequence = u32::from_be_bytes_slice(&buf[12..16])?;

    if header_size < HEADER_MIN_SIZE as u16 || header_size as u32 > total_size {
        return Err(anyhow::anyhow!("数据长度不足"));
    }

    Ok((total_size, header_size, version, opcode, sequence))
}

pub fn build_packet(version: u16, operation: u32, body: &[u8]) -> BytesMut {
    let header_length = HEADER_MIN_SIZE as u16;
    let total_length = (HEADER_MIN_SIZE + body.len()) as u32;

    let mut buf = BytesMut::with_capacity(total_length as usize);
    buf.put_u32(total_length);
    buf.put_u16(header_length);
    buf.put_u16(version);
    buf.put_u32(operation);
    buf.put_u32(DEFAULT_SEQUENCE);
    buf.extend_from_slice(body);
    buf
}

pub fn get_auth_packet(uid: u64, room_id: u64, token: String) -> Result<Vec<u8>> {
    let data = json!({
        "uid": uid,
        "roomid": room_id,
        "protover": 3,
        "platform": "web",
        "type": 2,
        "key": token,
    });
    let body = serde_json::to_vec(&data).context("序列化失败")?;
    Ok(build_packet(Version::HeartbeatAuth as u16, OpCode::Auth as u32, &body).to_vec())
}

pub fn get_heartbeat_packet() -> Vec<u8> {
    build_packet(Version::HeartbeatAuth as u16, OpCode::Heartbeat as u32, &[]).to_vec()
}

fn decompress_zlib(data: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = ZlibDecoder::new(data);
    let mut decompressed = Vec::new();
    decoder
        .read_to_end(&mut decompressed)
        .context("zlib 解压失败")?;
    Ok(decompressed)
}

fn decompress_brotli(data: &[u8]) -> Result<Vec<u8>> {
    let mut decompressed = Vec::new();
    Decompressor::new(data, data.len())
        .read_to_end(&mut decompressed)
        .context("brotli 解压失败")?;
    Ok(decompressed)
}

pub fn parse_packet(buf: &[u8]) -> Result<Vec<Packet>> {
    let (total_size, header_size, version, opcode, sequence) = parse_header(buf)?;

    if buf.len() < total_size as usize {
        return Err(anyhow::anyhow!("数据长度不足"));
    }

    let body = match version {
        2 => decompress_zlib(&buf[header_size as usize..total_size as usize])?,
        3 => decompress_brotli(&buf[header_size as usize..total_size as usize])?,
        _ => buf[header_size as usize..total_size as usize].to_vec(),
    };

    let mut packets = Vec::new();
    // brotli 压缩包可能包含多个包
    if version == 3 {
        packets.extend(parse_multi_packet(&body)?);
    } else {
        packets.push(Packet {
            total_size,
            header_size,
            version,
            opcode,
            sequence,
            body,
        });
    }

    Ok(packets)
}

fn parse_multi_packet(buf: &[u8]) -> Result<Vec<Packet>> {
    let mut packets = Vec::new();
    let mut offset = 0;

    while offset < buf.len() {
        let (total_size, header_size, version, opcode, sequence) = parse_header(&buf[offset..])?;

        if buf.len() - offset < total_size as usize {
            return Err(anyhow::anyhow!("数据长度不足"));
        }

        let body_start = offset + header_size as usize;
        let body_end = offset + total_size as usize;
        let body_slice = &buf[body_start..body_end];
        // 压缩只有一层
        packets.push(Packet {
            total_size,
            header_size,
            version,
            opcode,
            sequence,
            body: body_slice.to_vec(),
        });

        offset += total_size as usize;
    }

    Ok(packets)
}