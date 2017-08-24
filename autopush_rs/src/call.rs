//! Implementation of calling methods/objects in python
//!
//! The main `Server` has a channel that goes back to the main python thread,
//! and that's used to send instances of `PythonCall` from the Rust thread to
//! the Python thread. Typically you won't work with `PythonCall` directly
//! though but rather the various methods on the `Server` struct, documented
//! below. Each method will return a `MyFuture` of the result, representing the
//! decoded value from Python.
//!
//! Implementation-wise what's happening here is that each function call into
//! Python creates a `futures::sync::oneshot`. The `Sender` half of this oneshot
//! is sent to Python while the `Receiver` half stays in Rust. Arguments sent to
//! Python are serialized as JSON and arguments are received from Python as JSON
//! as well, meaning that they're deserialized in Rust from JSON as well.

use std::cell::RefCell;
use std::ffi::CStr;

use futures::Future;
use futures::sync::oneshot;
use libc::c_char;
use serde::de;
use serde::ser;
use serde_json;
use time::Tm;
use uuid::Uuid;

use errors::*;
use rt::{self, UnwindGuard, AutopushError};
use server::Server;

#[repr(C)]
pub struct AutopushPythonCall {
    inner: UnwindGuard<Inner>,
}

struct Inner {
    input: String,
    done: RefCell<Option<Box<FnBox>>>,
}

pub struct PythonCall {
    input: String,
    output: Box<FnBox>,
}

#[no_mangle]
pub extern "C" fn autopush_python_call_input_ptr(call: *mut AutopushPythonCall,
                                                 err: &mut AutopushError)
    -> *const u8
{
    unsafe {
        (*call).inner.catch(err, |call| {
            call.input.as_ptr()
        })
    }
}

#[no_mangle]
pub extern "C" fn autopush_python_call_input_len(call: *mut AutopushPythonCall,
                                                 err: &mut AutopushError)
    -> usize
{
    unsafe {
        (*call).inner.catch(err, |call| {
            call.input.len()
        })
    }
}

#[no_mangle]
pub extern "C" fn autopush_python_call_complete(call: *mut AutopushPythonCall,
                                                input: *const c_char,
                                                err: &mut AutopushError)
    -> i32
{
    unsafe {
        (*call).inner.catch(err, |call| {
            let input = CStr::from_ptr(input).to_str().unwrap();
            call.done.borrow_mut().take().unwrap().call(input);
        })
    }
}

#[no_mangle]
pub extern "C" fn autopush_python_call_free(call: *mut AutopushPythonCall) {
    rt::abort_on_panic(|| unsafe {
        Box::from_raw(call);
    })
}

impl AutopushPythonCall {
    pub fn new(call: PythonCall) -> AutopushPythonCall {
        AutopushPythonCall {
            inner: UnwindGuard::new(Inner {
                input: call.input,
                done: RefCell::new(Some(call.output)),
            }),
        }
    }

    fn _new<F>(input: String, f: F) -> AutopushPythonCall
        where F: FnOnce(&str) + Send + 'static,
    {
        AutopushPythonCall {
            inner: UnwindGuard::new(Inner {
                input: input,
                done: RefCell::new(Some(Box::new(f))),
            }),
        }
    }
}

trait FnBox: Send {
    fn call(self: Box<Self>, input: &str);
}

impl<F: FnOnce(&str) + Send> FnBox for F {
    fn call(self: Box<Self>, input: &str) {
        (*self)(input)
    }
}


#[derive(Serialize)]
#[serde(tag = "command", rename_all = "lowercase")]
enum Call<'a> {
    Hello {
        connected_at: i64,
        uaid: Option<&'a Uuid>,
    },

    CheckStorage {
        uaid: &'a Uuid,
        message_month: String,
        include_topic: bool,
        timestamp: Option<i64>,
    }
}

#[derive(Deserialize)]
struct PythonError {
    pub error: bool,
    pub error_msg: String,
}

#[derive(Deserialize)]
pub struct HelloResponse {
    pub uaid: Option<Uuid>,
    pub message_month: String,
    pub reset_uaid: bool,
    pub rotate_message_table: bool,
}

impl Server {
    pub fn hello(&self, connected_at: &Tm, uaid: Option<&Uuid>)
        -> MyFuture<HelloResponse>
    {
        let ms = (connected_at.tm_sec as i64 * 1000) +
                 (connected_at.tm_nsec as i64 / 1000 / 1000);
        let (call, fut) = PythonCall::new(&Call::Hello {
            connected_at: ms,
            uaid: uaid,
        });
        self.send_to_python(call);
        return fut
    }

    fn send_to_python(&self, call: PythonCall) {
        self.tx.send(Some(call)).expect("python went away?");
    }
}

impl PythonCall {
    fn new<T, U>(input: &T) -> (PythonCall, MyFuture<U>)
        where T: ser::Serialize,
              U: for<'de> de::Deserialize<'de> + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let call = PythonCall {
            input: serde_json::to_string(input).unwrap(),
            output: Box::new(|json: &str| {
                drop(tx.send(json_or_error(json)));
            }),
        };
        let rx = Box::new(rx.then(|res| {
            match res {
                Ok(Ok(s)) => Ok(serde_json::from_str(&s)?),
                Ok(Err(e)) => Err(e),
                Err(_) => Err("call canceled from python".into()),
            }
        }));
        (call, rx)
    }
}

fn json_or_error(json: &str) -> Result<String> {
    if let Ok(err) = serde_json::from_str::<PythonError>(json) {
        if err.error {
            return Err(format!("python exception: {}", err.error_msg).into())
        }
    }
    Ok(json.to_string())
}