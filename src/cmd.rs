use std::collections::HashMap;
use std::fmt::Display;

use bitflags::_core::cmp::max;
use bitflags::_core::fmt::{Debug, Formatter};

use crate::{Bytes, CstError};
use crate::type_counter::{decr_command, delcnt_command, incr_command};
use crate::lib::utils::bytes2i64;
use crate::link::Client;
use crate::type_hash::{deldict_command, hdel_command, hget_command, hgetall_command, hset_command};
use crate::type_set::{delset_command, sadd_command, smembers_command, spop_command, srem_command};
use crate::object::{Encoding, Object};
use crate::replica::{meet_command, replicas_command, sync_command};
use crate::resp::{Message, new_msg_ok};
use crate::stats::info_command;
use crate::resp::get_int_bytes;
use crate::server::Server;

pub type Range = (u32, u32);

#[derive(Debug)]
pub struct Cmd {
    args: Vec<Message>,
    command: &'static Command,
}

impl Display for Cmd {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("cmd_name: `{}`, args: [", self.command.name))?;
        for arg in self.args.iter() {
            f.write_fmt(format_args!("{}", arg))?;
        }
        f.write_str("]")
    }
}

impl Cmd {
    pub fn new(name: &[u8], args: Vec<Message>) -> Result<Cmd, CstError> {
        COMMANDS.get(name.to_ascii_lowercase().as_slice()).map(|c| Cmd{args, command: c}).ok_or(CstError::UnknownCmd(String::from(Bytes::from(name))))
    }

    pub fn exec(&self, client: Option<&mut Client>, server: &mut Server) -> Result<Message, CstError> {
        server.metrics.incr_cmd_processed();
        if self.command.flags & COMMAND_REPL_ONLY > 0 {
            return Err(CstError::UnknownCmd(self.command.name.to_string()));
        }
        let (nodeid, uuid) = {
            let is_write = self.command.flags | COMMAND_WRITE > 0;
            (server.node_id, server.next_uuid(is_write))
        };
        self.exec_detail(server, client, nodeid, uuid, (self.command.flags & COMMAND_WRITE) > 0 && (self.command.flags & COMMAND_NO_REPLICATE == 0))
    }

    // execute the command. when this function is call by replicate command, it does not duplicate again to other replicas.
    pub fn exec_detail(&self, server: &mut Server, client: Option<&mut Client>, nodeid: u64, uuid: u64, repl: bool) -> Result<Message, CstError> {
        let r = (self.command.handler)(server, client, nodeid, uuid, self.args.clone());
        debug!("Executed command {}, nodeid={}, uuid={}, repl={}, result={:?}", self, nodeid, uuid, repl, r);
        if !r.is_err() && repl {
            server.replicate_cmd(uuid, self.command.name, self.args.clone());
        }
        r
    }
}

type CommandHandler = fn(server: &mut Server, client: Option<&mut Client>, nodeid: u64, uuid: u64, args: Vec<Message>) -> Result<Message, CstError>;

pub struct Command {
    name: &'static str,
    handler: CommandHandler,
    flags: u16,
}

impl Debug for Command {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("Command{{name: {}}}", self.name))
    }
}

pub const COMMAND_READONLY: u16 = 1;
pub const COMMAND_WRITE: u16 = 1<<1;
pub const COMMAND_CTRL: u16 = 1<<2;
pub const COMMAND_NO_REPLICATE: u16 = 1<<3;
pub const COMMAND_NO_REPLY: u16 = 1<<4;
pub const COMMAND_REPL_ONLY: u16 = 1<<5;

macro_rules! new_command {
    ($table:expr, $name:expr, $handler: expr, $flags:expr) => {
        $table.insert($name.as_bytes(), Command{name: $name, handler: $handler, flags: $flags});
    };
}

lazy_static!{
    pub static ref COMMANDS: HashMap<&'static [u8], Command> = {
        let mut command_table = HashMap::new();
        // control
        new_command!(command_table, "node", node_command, COMMAND_CTRL);
        new_command!(command_table, "replicas", replicas_command, COMMAND_READONLY);
        new_command!(command_table, "sync", sync_command, COMMAND_CTRL);
        new_command!(command_table, "meet", meet_command, COMMAND_CTRL);
        new_command!(command_table, "client", client_command, COMMAND_CTRL);

        //stats
        new_command!(command_table, "repllog", repllog_command, COMMAND_READONLY);
        new_command!(command_table, "info", info_command, COMMAND_READONLY);

        // common commands
        new_command!(command_table, "get", get_command, COMMAND_READONLY);
        new_command!(command_table, "set", set_command, COMMAND_WRITE);
        new_command!(command_table, "desc", desc_command, COMMAND_READONLY);
        new_command!(command_table, "del", del_command, COMMAND_WRITE | COMMAND_NO_REPLICATE);
        new_command!(command_table, "delbytes", delbytes_command, COMMAND_WRITE | COMMAND_REPL_ONLY);

        // counter
        new_command!(command_table, "incr", incr_command, COMMAND_WRITE);
        new_command!(command_table, "decr", decr_command, COMMAND_WRITE);
        new_command!(command_table, "delcnt", delcnt_command, COMMAND_WRITE | COMMAND_REPL_ONLY);


        // set
        new_command!(command_table, "sadd", sadd_command, COMMAND_WRITE);
        new_command!(command_table, "srem", srem_command, COMMAND_WRITE);
        new_command!(command_table, "spop", spop_command, COMMAND_WRITE);
        new_command!(command_table, "smembers", smembers_command, COMMAND_READONLY);
        new_command!(command_table, "delset", delset_command, COMMAND_WRITE | COMMAND_REPL_ONLY);


        // dict
        new_command!(command_table, "hset", hset_command, COMMAND_WRITE);
        new_command!(command_table, "hget", hget_command, COMMAND_READONLY);
        new_command!(command_table, "hgetall", hgetall_command, COMMAND_READONLY);
        new_command!(command_table, "hdel", hdel_command, COMMAND_WRITE);
        new_command!(command_table, "deldict", deldict_command, COMMAND_WRITE | COMMAND_REPL_ONLY);


        command_table
    };
}


pub fn node_command(server: &mut Server, _client: Option<&mut Client>, _nodeid: u64, _uuid: u64, args: Vec<Message>) -> Result<Message, CstError> {
    let mut args = args.into_iter();
    let c_type = args.next_bytes()?;
    let v = args.next_bytes();
    match (c_type.as_bytes(), v) {
        (b"id", Err(_)) => Ok(Message::Integer(server.node_id as i64)),
        (b"id", Ok(r)) => {
            if let Some(i) = bytes2i64(r.as_bytes()) {
                if i > 0 {
                    server.node_id = i as u64;
                    return Ok(new_msg_ok());
                }
            }
            Ok(Message::Error("id must be greater than 0".into()))
        }
        (b"alias", Err(_)) => Ok(Message::BulkString(server.node_alias.as_bytes().into())),
        (b"alias", Ok(r)) => {
            server.node_alias = String::from(r);
            Ok(new_msg_ok())
        }
        _ => {
            Ok(Message::Error("unsupported command".into()))
        }
    }
}

pub fn get_command(server: &mut Server, _client: Option<&mut Client>, _nodeid: u64, uuid: u64, args: Vec<Message>) -> Result<Message, CstError> {
    info!("thread id: {:?}", std::thread::current().id());
    let mut args = args.into_iter();
    let key_name = args.next_bytes()?;
    match server.db.query(&key_name, uuid) {
        Some(o) => {
            if o.create_time < o.delete_time {
                return Ok(Message::Nil);
            }
            match &o.enc {
                Encoding::Counter(c) => Ok(Message::Integer(c.get())),
                Encoding::Bytes(b) => Ok(Message::BulkString(b.clone())),
                _ => Err(CstError::InvalidType)
            }
        }
        None => Ok(Message::Nil),
    }
}

pub fn set_command(server: &mut Server, _client: Option<&mut Client>, _nodeid: u64, uuid: u64, args: Vec<Message>) -> Result<Message, CstError> {
    let mut args = args.into_iter();
    let key_name = args.next_bytes()?;
    let value = args.next_bytes()?;
    // let o = server.db.entry(key_name).or_insert(Object::new(Encoding::Bytes(value.clone()), uuid, 0));
    let o = match server.db.query(&key_name, uuid) {
        None => {
            let o = Object::new(Encoding::Bytes(value.clone()), uuid, 0);
            server.db.add(key_name.clone(), o);
            server.db.query(&key_name, uuid).unwrap()
        }
        Some(o) => o,
    };
    if o.update_time > uuid {
        return Ok(Message::Integer(0));
    }
    match o.enc {
        Encoding::Bytes(_) => {},
        _ => return Err(CstError::InvalidType),
    }
    o.enc = Encoding::Bytes(value);
    o.updated_at(uuid);
    Ok(new_msg_ok())
}

pub fn desc_command(server: &mut Server, _client: Option<&mut Client>, _nodeid: u64, uuid: u64, args: Vec<Message>) -> Result<Message, CstError> {
    let mut args = args.into_iter();
    let key_name = args.next_bytes()?;
    match server.db.query(&key_name, uuid) {
        None => Ok(Message::Nil),
        Some(o) => Ok(o.describe())
    }
}

// del command can be sent only by the client, not the replicas.
pub fn del_command(server: &mut Server, _client: Option<&mut Client>, _nodeid: u64, uuid: u64, args: Vec<Message>) -> Result<Message, CstError> {
    let mut args = args.into_iter();
    let mut deleted = 0;
    let mut replicates = vec![];
    let key_name = args.next_bytes()?;
    match server.db.query(&key_name, uuid) {
        None => {},
        Some(v) => {
            debug!("deleting object, ct: {}, dt: {}, mt: {}", v.create_time, v.delete_time, v.update_time);
            match &mut v.enc {
                // as for counter and bytes, we don't allow deletion before some later modifications exist already.
                // since we are sure that the `del` command is sent by our clients, not replicas, this policy doesn't ruin our eventual consistency.
                Encoding::Counter(g) => {
                    if v.update_time <= uuid { // v.ct and v.dt must be less than uuid
                        if v.create_time < v.delete_time {
                            // already deleted, and has no following modifications since that deletion
                        } else {
                            v.delete_time = uuid;
                            v.update_time = uuid;
                            deleted = 1;
                            let mut d = HashMap::new();
                            for (nodeid, (value, _)) in g.iter() {
                                d.insert(nodeid, value);
                            }
                            let mut args = Vec::with_capacity(d.len() * 2 + 1);
                            args.push(Message::BulkString(key_name.into()));
                            for (nodeid, value) in d {
                                g.change(nodeid, -value, uuid);
                                args.push(Message::Integer(nodeid as i64));
                                args.push(Message::Integer(-value));
                            }
                            replicates.push(("delcnt", args));
                        }
                    }
                }
                Encoding::Bytes(_) => {
                    if v.update_time <= uuid { // v.ct and v.dt must be less than uuid
                        if v.create_time < v.delete_time {  // already deleted

                        } else {
                            v.delete_time = uuid;
                            v.update_time = uuid;
                            deleted = 1;
                            replicates.push(("delbytes", vec![Message::BulkString(key_name.into())]));
                        }
                    }
                }
                Encoding::LWWSet(s) => {
                    let members: Vec<Bytes> = s.iter_all().map(|(x, _)| x.clone()).collect();
                    let _ = s.remove_members(members.as_slice(), uuid);
                    if v.create_time >= v.delete_time && uuid > v.create_time {  // exist before and now deleted
                        deleted = 1;
                    }
                    v.delete_time = max(v.delete_time, uuid);
                    v.update_time = max(v.update_time, uuid);
                    replicates.push(("delset", vec![Message::BulkString(key_name.into())]));
                }
                Encoding::LWWDict(d) => {
                    let fields: Vec<Bytes> = d.iter_all().map(|(b, _, _)| b.clone()).collect();
                    let _ = d.del_fields(fields.as_slice(), uuid);
                    if v.create_time >= v.delete_time && uuid > v.create_time { // exist before and now deleted
                        deleted = 1;
                    }
                    v.delete_time = max(v.delete_time, uuid);
                    v.update_time = max(v.update_time, uuid);
                    replicates.push(("deldict", vec![Message::BulkString(key_name.into())]));
                }
            }
        }
    }

    for (cmd, args) in replicates {
        server.replicate_cmd(uuid, cmd, args);
    }
    Ok(Message::Integer(deleted))
}

pub fn delbytes_command(server: &mut Server, _client: Option<&mut Client>, _nodeid: u64, uuid: u64, args: Vec<Message>) -> Result<Message, CstError> {
    let mut args = args.into_iter();
    let key_name = args.next_bytes()?;
    //let o = server.db.entry(key_name).or_insert(Object::new(Encoding::Bytes("".into()), uuid, 0));
    let o = match server.db.query(&key_name, uuid) {
        None => {
            let o = Object::new(Encoding::Bytes("".into()), uuid, 0);
            server.db.add(key_name.clone(), o);
            server.db.query(&key_name, uuid).unwrap()
        }
        Some(o) => o,
    };
    match o.enc {
        Encoding::Bytes(_) => {},
        _ => return Err(CstError::InvalidType),
    }
    o.delete_time = max(o.delete_time, uuid);
    o.update_time = max(o.update_time, uuid);
    Ok(Message::None)
}

pub fn repllog_command(server: &mut Server, _client: Option<&mut Client>, _nodeid: u64, _uuid: u64, args: Vec<Message>) -> Result<Message, CstError> {
    let mut args = args.into_iter();
    let sub_command = args.next_string()?;
    match sub_command.to_ascii_lowercase().as_str() {
        "at" => {
            let uuid = args.next_u64()?;
            Ok(server.repl_log_at(uuid).unwrap_or(Message::Nil))
        }
        "uuids" => {
            let uuids: Vec<Message> = server.repl_log_uuids().into_iter().map(|x| Message::Integer(x as i64)).collect();
            Ok(Message::Array(uuids))

        }
        others => Err(CstError::UnknownSubCmd(others.to_string(), "REPLLOG".to_string())),
    }
}

pub fn client_command(_server: &mut Server, client: Option<&mut Client>, _nodeid: u64, _uuid: u64, args: Vec<Message>) -> Result<Message, CstError> {
    let mut args = args.into_iter();
    let sub_command = args.next_string()?;
    match sub_command.to_ascii_lowercase().as_str() {
        "threadid" => {
            let tid = client.unwrap().thread_id;
            Ok(Message::BulkString(format!("{:?}", tid).into()))
        }
        others => Err(CstError::UnknownSubCmd(others.to_string(), "CLIENT".to_string())),
    }
}

pub trait NextArg {
    fn next_arg(&mut self) -> Result<Message, CstError>;
    fn next_bytes(&mut self) -> Result<Bytes, CstError>;
    fn next_i64(&mut self) -> Result<i64, CstError>;
    fn next_u64(&mut self) -> Result<u64, CstError>;
    fn next_string(&mut self) -> Result<String, CstError>;
}

impl<T> NextArg for T
    where
        T: Iterator<Item = Message>,
{
    fn next_arg(&mut self) -> Result<Message, CstError> {
        self.next().map_or(Err(CstError::WrongArity), |x| Ok(x))
    }

    fn next_bytes(&mut self) -> Result<Bytes, CstError> {
        match self.next_arg()? {
            Message::Integer(i) => Ok(get_int_bytes(i)),
            Message::Error(e) => Ok(e),
            Message::String(s) => Ok(s),
            Message::BulkString(b) => Ok(b),
            _ => Err(CstError::InvalidRequestMsg("should be non-array type".to_string())),
        }
    }

    fn next_i64(&mut self) -> Result<i64, CstError> {
        match self.next_arg()? {
            Message::Integer(i) => Ok(i),
            Message::String(s) => bytes2i64(s.as_bytes()).ok_or(CstError::InvalidRequestMsg("string should be an integer".to_string())),
            Message::BulkString(s) => bytes2i64(s.as_bytes()).ok_or(CstError::InvalidRequestMsg("bulk string should be an integer".to_string())),
            _ => Err(CstError::InvalidRequestMsg("argument should be of type Integer or String or BulkString".to_string())),
        }
    }

    fn next_u64(&mut self) -> Result<u64, CstError> {
        match self.next_i64() {
            Ok(i) => if i >= 0 {
                Ok(i as u64)
            } else {
                Err(CstError::InvalidRequestMsg("argument should be an unsigned integer".to_string()))
            }
            Err(e) => Err(e)
        }
    }

    fn next_string(&mut self) -> Result<String, CstError> {
        self.next_bytes().map(|x| x.into())
    }
}
