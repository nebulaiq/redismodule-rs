use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_long, c_longlong};
use std::ptr;

use crate::key::{RedisKey, RedisKeyWritable};
use crate::raw::{ModuleOptions, Version};
use crate::{add_info_field_long_long, add_info_field_str, raw, utils, Status};
use crate::{add_info_section, LogLevel};
use crate::{RedisError, RedisResult, RedisString, RedisValue};

use std::collections::{HashMap, HashSet};

#[cfg(feature = "experimental-api")]
use std::ffi::CStr;

#[cfg(feature = "experimental-api")]
mod timer;

#[cfg(feature = "experimental-api")]
pub mod thread_safe;

#[cfg(feature = "experimental-api")]
pub mod blocked;

pub mod info;

pub mod keys_cursor;

pub mod server_events;

pub mod configuration;

#[derive(Clone)]
pub struct CallOptions {
    options: String,
}

// TODO rewrite as a bitfield which is serializable to a string.
// This will help a lot to simplify the code and make it more developer-
// friendly, also will avoid possible duplicates and will consume less
// space, also will be allocated on stack instead of the heap.
#[derive(Debug, Clone)]
pub struct CallOptionsBuilder {
    options: String,
}

impl Default for CallOptionsBuilder {
    fn default() -> Self {
        CallOptionsBuilder {
            options: "v".to_string(),
        }
    }
}

impl CallOptionsBuilder {
    pub fn new() -> CallOptionsBuilder {
        Self::default()
    }

    fn add_flag(&mut self, flag: &str) {
        self.options.push_str(flag);
    }

    pub fn no_writes(mut self) -> CallOptionsBuilder {
        self.add_flag("W");
        self
    }

    pub fn script_mode(mut self) -> CallOptionsBuilder {
        self.add_flag("S");
        self
    }

    pub fn verify_acl(mut self) -> CallOptionsBuilder {
        self.add_flag("C");
        self
    }

    pub fn verify_oom(mut self) -> CallOptionsBuilder {
        self.add_flag("M");
        self
    }

    pub fn errors_as_replies(mut self) -> CallOptionsBuilder {
        self.add_flag("E");
        self
    }

    pub fn replicate(mut self) -> CallOptionsBuilder {
        self.add_flag("!");
        self
    }

    pub fn resp_3(mut self) -> CallOptionsBuilder {
        self.add_flag("3");
        self
    }

    pub fn constract(&self) -> CallOptions {
        let mut res = CallOptions {
            options: self.options.to_string(),
        };
        // TODO don't "make" it a C string, just use a [CString].
        res.options.push('\0'); /* make it C string */
        res
    }
}

// TODO rewrite using the bit_fields crate.
#[derive(Debug, Default, Copy, Clone)]
pub struct AclPermissions {
    flags: u32,
}

impl AclPermissions {
    pub fn new() -> AclPermissions {
        Self::default()
    }

    pub fn add_access_permission(&mut self) {
        self.flags |= raw::REDISMODULE_CMD_KEY_ACCESS;
    }

    pub fn add_insert_permission(&mut self) {
        self.flags |= raw::REDISMODULE_CMD_KEY_INSERT;
    }

    pub fn add_delete_permission(&mut self) {
        self.flags |= raw::REDISMODULE_CMD_KEY_DELETE;
    }

    pub fn add_update_permission(&mut self) {
        self.flags |= raw::REDISMODULE_CMD_KEY_UPDATE;
    }

    pub fn add_full_permission(&mut self) {
        self.add_access_permission();
        self.add_insert_permission();
        self.add_delete_permission();
        self.add_update_permission();
    }
}

/// `Context` is a structure that's designed to give us a high-level interface to
/// the Redis module API by abstracting away the raw C FFI calls.
pub struct Context {
    pub ctx: *mut raw::RedisModuleCtx,
}

impl Context {
    pub const fn new(ctx: *mut raw::RedisModuleCtx) -> Self {
        Self { ctx }
    }

    #[must_use]
    pub const fn dummy() -> Self {
        Self {
            ctx: ptr::null_mut(),
        }
    }

    pub fn log(&self, level: LogLevel, message: &str) {
        crate::logging::log_internal(self.ctx, level, message);
    }

    pub fn log_debug(&self, message: &str) {
        self.log(LogLevel::Debug, message);
    }

    pub fn log_notice(&self, message: &str) {
        self.log(LogLevel::Notice, message);
    }

    pub fn log_verbose(&self, message: &str) {
        self.log(LogLevel::Verbose, message);
    }

    pub fn log_warning(&self, message: &str) {
        self.log(LogLevel::Warning, message);
    }

    /// # Panics
    ///
    /// Will panic if `RedisModule_AutoMemory` is missing in redismodule.h
    pub fn auto_memory(&self) {
        unsafe {
            raw::RedisModule_AutoMemory.unwrap()(self.ctx);
        }
    }

    /// # Panics
    ///
    /// Will panic if `RedisModule_IsKeysPositionRequest` is missing in redismodule.h
    #[must_use]
    pub fn is_keys_position_request(&self) -> bool {
        // We want this to be available in tests where we don't have an actual Redis to call
        if cfg!(feature = "test") {
            return false;
        }

        let result = unsafe { raw::RedisModule_IsKeysPositionRequest.unwrap()(self.ctx) };

        result != 0
    }

    /// # Panics
    ///
    /// Will panic if `RedisModule_KeyAtPos` is missing in redismodule.h
    pub fn key_at_pos(&self, pos: i32) {
        // TODO: This will crash redis if `pos` is out of range.
        // Think of a way to make this safe by checking the range.
        unsafe {
            raw::RedisModule_KeyAtPos.unwrap()(self.ctx, pos as c_int);
        }
    }

    // The lint is disabled since all the behaviour is controlled via Redis,
    // and all the pointers if dereferenced will be dereferenced by the module.
    //
    // Since we can't know the logic of Redis when it comes to pointers, we
    // can't say whether passing a null pointer is okay to a redis function
    // or not. So we can neither deny it is valid nor confirm.
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    pub fn call_internal(
        &self,
        command: &str,
        options: *const c_char,
        args: &[&[u8]],
    ) -> RedisResult {
        let terminated_args: Vec<RedisString> = args
            .iter()
            .map(|s| RedisString::create_from_slice(self.ctx, s))
            .collect();

        let inner_args: Vec<*mut raw::RedisModuleString> =
            terminated_args.iter().map(|s| s.inner).collect();

        let cmd = CString::new(command).unwrap();
        let reply: *mut raw::RedisModuleCallReply = unsafe {
            let p_call = raw::RedisModule_Call.unwrap();
            p_call(
                self.ctx,
                cmd.as_ptr(),
                options,
                inner_args.as_ptr() as *mut c_char,
                terminated_args.len(),
            )
        };
        let result = Self::parse_call_reply(reply);
        if !reply.is_null() {
            raw::free_call_reply(reply);
        }
        result
    }

    pub fn call_ext(&self, command: &str, options: &CallOptions, args: &[&[u8]]) -> RedisResult {
        self.call_internal(command, options.options.as_ptr() as *const c_char, args)
    }

    pub fn call(&self, command: &str, args: &[&str]) -> RedisResult {
        self.call_internal(
            command,
            raw::FMT,
            &args.iter().map(|v| v.as_bytes()).collect::<Vec<&[u8]>>(),
        )
    }

    fn parse_call_reply(reply: *mut raw::RedisModuleCallReply) -> RedisResult {
        match raw::call_reply_type(reply) {
            raw::ReplyType::Error => Err(RedisError::String(raw::call_reply_string(reply))),
            raw::ReplyType::Unknown => Err(RedisError::Str("Error on method call")),
            raw::ReplyType::Array => {
                let length = raw::call_reply_length(reply);
                let mut vec = Vec::with_capacity(length);
                for i in 0..length {
                    vec.push(Self::parse_call_reply(raw::call_reply_array_element(
                        reply, i,
                    ))?);
                }
                Ok(RedisValue::Array(vec))
            }
            raw::ReplyType::Integer => Ok(RedisValue::Integer(raw::call_reply_integer(reply))),
            raw::ReplyType::String => Ok(RedisValue::StringBuffer({
                let mut len: usize = 0;
                let buff = raw::call_reply_string_ptr(reply, &mut len);
                unsafe { std::slice::from_raw_parts(buff as *mut u8, len) }.to_vec()
            })),
            raw::ReplyType::Null => Ok(RedisValue::Null),
            raw::ReplyType::Map => {
                let length = raw::call_reply_length(reply);
                let mut map = HashMap::new();
                for i in 0..length {
                    let (key, val) = raw::call_reply_map_element(reply, i);
                    let key = Self::parse_call_reply(key)?;
                    let val = Self::parse_call_reply(val)?;
                    // The numbers are converted to a string, it is probably
                    // good enough for most usecases and the effort to support
                    // it as number is big.
                    let key = match key {
                        RedisValue::SimpleString(s) => s.as_bytes().to_vec(),
                        RedisValue::SimpleStringStatic(s) => s.as_bytes().to_vec(),
                        RedisValue::BulkString(s) => s.as_bytes().to_vec(),
                        RedisValue::BulkRedisString(s) => s.as_slice().to_vec(),
                        RedisValue::Integer(i) => i.to_string().as_bytes().to_vec(),
                        RedisValue::Float(f) => f.to_string().as_bytes().to_vec(),
                        RedisValue::StringBuffer(b) => b,
                        _ => return Err(RedisError::Str("type is not supported as map key")),
                    };
                    map.insert(key, val);
                }
                Ok(RedisValue::Map(map))
            }
            raw::ReplyType::Set => {
                let length = raw::call_reply_length(reply);
                let mut set = HashSet::new();
                for i in 0..length {
                    let val = raw::call_reply_set_element(reply, i);
                    let val = Self::parse_call_reply(val)?;
                    // The numbers are converted to a string, it is probably
                    // good enough for most usecases and the effort to support
                    // it as number is big.
                    let val = match val {
                        RedisValue::SimpleString(s) => s.as_bytes().to_vec(),
                        RedisValue::SimpleStringStatic(s) => s.as_bytes().to_vec(),
                        RedisValue::BulkString(s) => s.as_bytes().to_vec(),
                        RedisValue::BulkRedisString(s) => s.as_slice().to_vec(),
                        RedisValue::Integer(i) => i.to_string().as_bytes().to_vec(),
                        RedisValue::Float(f) => f.to_string().as_bytes().to_vec(),
                        RedisValue::StringBuffer(b) => b,
                        _ => return Err(RedisError::Str("type is not supported on set")),
                    };
                    set.insert(val);
                }
                Ok(RedisValue::Set(set))
            }
            raw::ReplyType::Bool => Ok(RedisValue::Bool(raw::call_reply_bool(reply) != 0)),
            raw::ReplyType::Double => Ok(RedisValue::Double(raw::call_reply_double(reply))),
            raw::ReplyType::BigNumber => {
                Ok(RedisValue::BigNumber(raw::call_reply_big_numebr(reply)))
            }
            raw::ReplyType::VerbatimString => Ok(RedisValue::VerbatimString(
                raw::call_reply_verbatim_string(reply),
            )),
        }
    }

    #[must_use]
    pub fn str_as_legal_resp_string(s: &str) -> CString {
        CString::new(
            s.chars()
                .map(|c| match c {
                    '\r' | '\n' | '\0' => b' ',
                    _ => c as u8,
                })
                .collect::<Vec<_>>(),
        )
        .unwrap()
    }

    #[allow(clippy::must_use_candidate)]
    pub fn reply_null(&self) -> raw::Status {
        unsafe { raw::RedisModule_ReplyWithNull.unwrap()(self.ctx).into() }
    }

    #[allow(clippy::must_use_candidate)]
    pub fn reply_simple_string(&self, s: &str) -> raw::Status {
        let msg = Self::str_as_legal_resp_string(s);
        unsafe { raw::RedisModule_ReplyWithSimpleString.unwrap()(self.ctx, msg.as_ptr()).into() }
    }

    #[allow(clippy::must_use_candidate)]
    pub fn reply_bulk_string(&self, s: &str) -> raw::Status {
        unsafe {
            raw::RedisModule_ReplyWithStringBuffer.unwrap()(
                self.ctx,
                s.as_ptr() as *mut c_char,
                s.len(),
            )
            .into()
        }
    }

    #[allow(clippy::must_use_candidate)]
    pub fn reply_bulk_slice(&self, s: &[u8]) -> raw::Status {
        unsafe {
            raw::RedisModule_ReplyWithStringBuffer.unwrap()(
                self.ctx,
                s.as_ptr() as *mut c_char,
                s.len(),
            )
            .into()
        }
    }

    #[allow(clippy::must_use_candidate)]
    pub fn reply_array(&self, size: usize) -> raw::Status {
        unsafe { raw::RedisModule_ReplyWithArray.unwrap()(self.ctx, size as c_long).into() }
    }

    #[allow(clippy::must_use_candidate)]
    pub fn reply_long(&self, l: i64) -> raw::Status {
        unsafe { raw::RedisModule_ReplyWithLongLong.unwrap()(self.ctx, l as c_longlong).into() }
    }

    #[allow(clippy::must_use_candidate)]
    pub fn reply_double(&self, d: f64) -> raw::Status {
        unsafe { raw::RedisModule_ReplyWithDouble.unwrap()(self.ctx, d).into() }
    }

    #[allow(clippy::must_use_candidate)]
    pub fn reply_error_string(&self, s: &str) -> raw::Status {
        let msg = Self::str_as_legal_resp_string(s);
        unsafe { raw::RedisModule_ReplyWithError.unwrap()(self.ctx, msg.as_ptr()).into() }
    }

    /// # Panics
    ///
    /// Will panic if methods used are missing in redismodule.h
    #[allow(clippy::must_use_candidate)]
    pub fn reply(&self, r: RedisResult) -> raw::Status {
        match r {
            Ok(RedisValue::Integer(v)) => unsafe {
                raw::RedisModule_ReplyWithLongLong.unwrap()(self.ctx, v).into()
            },

            Ok(RedisValue::Float(v)) => unsafe {
                raw::RedisModule_ReplyWithDouble.unwrap()(self.ctx, v).into()
            },

            Ok(RedisValue::SimpleStringStatic(s)) => unsafe {
                let msg = CString::new(s).unwrap();
                raw::RedisModule_ReplyWithSimpleString.unwrap()(self.ctx, msg.as_ptr()).into()
            },

            Ok(RedisValue::SimpleString(s)) => unsafe {
                let msg = CString::new(s).unwrap();
                raw::RedisModule_ReplyWithSimpleString.unwrap()(self.ctx, msg.as_ptr()).into()
            },

            Ok(RedisValue::BulkString(s)) => unsafe {
                raw::RedisModule_ReplyWithStringBuffer.unwrap()(
                    self.ctx,
                    s.as_ptr().cast(),
                    s.len(),
                )
                .into()
            },

            Ok(RedisValue::BulkRedisString(s)) => unsafe {
                raw::RedisModule_ReplyWithString.unwrap()(self.ctx, s.inner).into()
            },

            Ok(RedisValue::StringBuffer(s)) => unsafe {
                raw::RedisModule_ReplyWithStringBuffer.unwrap()(
                    self.ctx,
                    s.as_ptr().cast(),
                    s.len(),
                )
                .into()
            },

            Ok(RedisValue::Array(array)) => {
                unsafe {
                    // According to the Redis source code this always succeeds,
                    // so there is no point in checking its return value.
                    raw::RedisModule_ReplyWithArray.unwrap()(self.ctx, array.len() as c_long);
                }

                for elem in array {
                    self.reply(Ok(elem));
                }

                raw::Status::Ok
            }

            Ok(RedisValue::Map(map)) => {
                unsafe {
                    raw::RedisModule_ReplyWithMap.unwrap()(self.ctx, map.len() as c_long);
                }

                for (key, val) in map {
                    unsafe {
                        raw::RedisModule_ReplyWithStringBuffer.unwrap()(
                            self.ctx,
                            key.as_ptr().cast(),
                            key.len(),
                        );
                    };
                    self.reply(Ok(val));
                }

                raw::Status::Ok
            }

            Ok(RedisValue::Set(set)) => {
                unsafe {
                    raw::RedisModule_ReplyWithSet.unwrap()(self.ctx, set.len() as c_long);
                }

                for val in set {
                    unsafe {
                        raw::RedisModule_ReplyWithStringBuffer.unwrap()(
                            self.ctx,
                            val.as_ptr().cast(),
                            val.len(),
                        );
                    };
                }

                raw::Status::Ok
            }

            Ok(RedisValue::Bool(b)) => unsafe {
                raw::RedisModule_ReplyWithBool.unwrap()(self.ctx, b as c_int).into()
            },

            Ok(RedisValue::Double(d)) => unsafe {
                raw::RedisModule_ReplyWithDouble.unwrap()(self.ctx, d).into()
            },

            Ok(RedisValue::BigNumber(s)) => unsafe {
                raw::RedisModule_ReplyWithBigNumber.unwrap()(
                    self.ctx,
                    s.as_ptr() as *mut c_char,
                    s.len(),
                )
                .into()
            },

            Ok(RedisValue::VerbatimString((t, s))) => unsafe {
                raw::RedisModule_ReplyWithVerbatimStringType.unwrap()(
                    self.ctx,
                    s.as_ptr() as *mut c_char,
                    s.len(),
                    t.as_ptr() as *mut c_char,
                )
                .into()
            },

            Ok(RedisValue::Null) => unsafe {
                raw::RedisModule_ReplyWithNull.unwrap()(self.ctx).into()
            },

            Ok(RedisValue::NoReply) => raw::Status::Ok,

            Err(RedisError::WrongArity) => unsafe {
                if self.is_keys_position_request() {
                    // We can't return a result since we don't have a client
                    raw::Status::Err
                } else {
                    raw::RedisModule_WrongArity.unwrap()(self.ctx).into()
                }
            },

            Err(RedisError::WrongType) => {
                self.reply_error_string(RedisError::WrongType.to_string().as_str())
            }

            Err(RedisError::String(s)) => self.reply_error_string(s.as_str()),

            Err(RedisError::Str(s)) => self.reply_error_string(s),
        }
    }

    #[must_use]
    pub fn open_key(&self, key: &RedisString) -> RedisKey {
        RedisKey::open(self.ctx, key)
    }

    #[must_use]
    pub fn open_key_writable(&self, key: &RedisString) -> RedisKeyWritable {
        RedisKeyWritable::open(self.ctx, key)
    }

    pub fn replicate_verbatim(&self) {
        raw::replicate_verbatim(self.ctx);
    }

    #[must_use]
    pub fn create_string(&self, s: &str) -> RedisString {
        RedisString::create(self.ctx, s)
    }

    #[must_use]
    pub fn create_string_from_slice(&self, s: &[u8]) -> RedisString {
        RedisString::create_from_slice(self.ctx, s)
    }

    #[must_use]
    pub const fn get_raw(&self) -> *mut raw::RedisModuleCtx {
        self.ctx
    }

    /// # Safety
    #[cfg(feature = "experimental-api")]
    pub unsafe fn export_shared_api(
        &self,
        func: *const ::std::os::raw::c_void,
        name: *const ::std::os::raw::c_char,
    ) {
        raw::export_shared_api(self.ctx, func, name);
    }

    #[cfg(feature = "experimental-api")]
    #[allow(clippy::must_use_candidate)]
    pub fn notify_keyspace_event(
        &self,
        event_type: raw::NotifyEvent,
        event: &str,
        keyname: &RedisString,
    ) -> raw::Status {
        unsafe { raw::notify_keyspace_event(self.ctx, event_type, event, keyname) }
    }

    #[cfg(feature = "experimental-api")]
    pub fn current_command_name(&self) -> Result<String, RedisError> {
        unsafe {
            match raw::RedisModule_GetCurrentCommandName {
                Some(cmd) => Ok(CStr::from_ptr(cmd(self.ctx)).to_str().unwrap().to_string()),
                None => Err(RedisError::Str(
                    "API RedisModule_GetCurrentCommandName is not available",
                )),
            }
        }
    }

    /// Returns the redis version either by calling RedisModule_GetServerVersion API,
    /// Or if it is not available, by calling "info server" API and parsing the reply
    pub fn get_redis_version(&self) -> Result<Version, RedisError> {
        self.get_redis_version_internal(false)
    }

    /// Returns the redis version by calling "info server" API and parsing the reply
    #[cfg(feature = "test")]
    pub fn get_redis_version_rm_call(&self) -> Result<Version, RedisError> {
        self.get_redis_version_internal(true)
    }

    pub fn version_from_info(info: RedisValue) -> Result<Version, RedisError> {
        let info_str = match info {
            RedisValue::SimpleString(info_str) => info_str,
            RedisValue::StringBuffer(b) => std::str::from_utf8(&b).unwrap().to_string(),
            _ => return Err(RedisError::Str("Error getting redis_version")),
        };
        if let Some(ver) = utils::get_regexp_captures(
            info_str.as_str(),
            r"(?m)\bredis_version:([0-9]+)\.([0-9]+)\.([0-9]+)\b",
        ) {
            return Ok(Version {
                major: ver[0].parse::<c_int>().unwrap(),
                minor: ver[1].parse::<c_int>().unwrap(),
                patch: ver[2].parse::<c_int>().unwrap(),
            });
        }
        Err(RedisError::Str("Error getting redis_version"))
    }

    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    fn get_redis_version_internal(&self, force_use_rm_call: bool) -> Result<Version, RedisError> {
        match unsafe { raw::RedisModule_GetServerVersion } {
            Some(api) if !force_use_rm_call => {
                // Call existing API
                Ok(Version::from(unsafe { api() }))
            }
            _ => {
                // Call "info server"
                if let Ok(info) = self.call("info", &["server"]) {
                    Context::version_from_info(info)
                } else {
                    Err(RedisError::Str("Error calling \"info server\""))
                }
            }
        }
    }

    pub fn set_module_options(&self, options: ModuleOptions) {
        unsafe { raw::RedisModule_SetModuleOptions.unwrap()(self.ctx, options.bits()) };
    }

    pub fn is_primary(&self) -> bool {
        let flags = unsafe { raw::RedisModule_GetContextFlags.unwrap()(self.ctx) };
        flags as u32 & raw::REDISMODULE_CTX_FLAGS_MASTER != 0
    }

    pub fn is_oom(&self) -> bool {
        let flags = unsafe { raw::RedisModule_GetContextFlags.unwrap()(self.ctx) };
        flags as u32 & raw::REDISMODULE_CTX_FLAGS_OOM != 0
    }

    pub fn allow_block(&self) -> bool {
        let flags = unsafe { raw::RedisModule_GetContextFlags.unwrap()(self.ctx) };
        (flags as u32 & raw::REDISMODULE_CTX_FLAGS_DENY_BLOCKING) == 0
    }

    pub fn get_current_user(&self) -> Result<String, RedisError> {
        let user = unsafe { raw::RedisModule_GetCurrentUserName.unwrap()(self.ctx) };
        let user = RedisString::from_redis_module_string(ptr::null_mut(), user);
        Ok(user.try_as_str()?.to_string())
    }

    pub fn autenticate_user(&self, user_name: &str) -> raw::Status {
        if unsafe {
            raw::RedisModule_AuthenticateClientWithACLUser.unwrap()(
                self.ctx,
                user_name.as_ptr() as *const c_char,
                user_name.len(),
                None,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        } == raw::REDISMODULE_OK as i32
        {
            raw::Status::Ok
        } else {
            raw::Status::Err
        }
    }

    pub fn acl_check_key_permission(
        &self,
        user_name: &str,
        key_name: &RedisString,
        permissions: &AclPermissions,
    ) -> Result<(), RedisError> {
        let user_name = RedisString::create(self.ctx, user_name);
        let user = unsafe { raw::RedisModule_GetModuleUserFromUserName.unwrap()(user_name.inner) };
        if user.is_null() {
            return Err(RedisError::Str("User does not exists or disabled"));
        }
        if unsafe {
            raw::RedisModule_ACLCheckKeyPermissions.unwrap()(
                user,
                key_name.inner,
                permissions.flags as i32,
            )
        } == raw::REDISMODULE_OK as i32
        {
            unsafe { raw::RedisModule_FreeModuleUser.unwrap()(user) };
            Ok(())
        } else {
            unsafe { raw::RedisModule_FreeModuleUser.unwrap()(user) };
            Err(RedisError::Str("User does not have permissions on key"))
        }
    }
}

pub struct InfoContext {
    pub ctx: *mut raw::RedisModuleInfoCtx,
}

impl InfoContext {
    pub const fn new(ctx: *mut raw::RedisModuleInfoCtx) -> Self {
        Self { ctx }
    }

    #[allow(clippy::must_use_candidate)]
    pub fn add_info_section(&self, name: Option<&str>) -> Status {
        add_info_section(self.ctx, name)
    }

    #[allow(clippy::must_use_candidate)]
    pub fn add_info_field_str(&self, name: &str, content: &str) -> Status {
        add_info_field_str(self.ctx, name, content)
    }

    #[allow(clippy::must_use_candidate)]
    pub fn add_info_field_long_long(&self, name: &str, value: c_longlong) -> Status {
        add_info_field_long_long(self.ctx, name, value)
    }
}
