use crate::error;
use log;
use ordered_float;
use crate::query;
use std::cell;
use std::collections;
use std::mem;
use std::str;
use v8;
use crate::value;

const API_JS: &'static str = include_str!("../api.js");
const PRELUDE_JS: &'static str = include_str!("../prelude.js");

const MODULES: &'static [(&'static str, &'static str)] =
    &[("lodash", include_str!("../js/lodash.js"))];

#[derive(Debug)]
pub struct Context {
    isolate: v8::Isolate,
}

#[derive(Debug)]
pub struct Process<'a> {
    context: &'a Context,
    v8_context: v8::Context,
    resume: v8::value::Function,
    pub state: State,
}

#[derive(Debug)]
pub enum State {
    Start,
    Pending,
    Await,
    Emit(value::Value),
    End,
}

#[derive(Debug)]
pub enum Resume {
    Start(ResumeStart),
    Pending(ResumePending),
    Await(ResumeAwait),
    Emit(ResumeEmit),
    End,
}

#[derive(Debug)]
pub struct ResumeStart;

#[derive(Debug)]
pub struct ResumePending;

#[derive(Debug)]
pub struct ResumeAwait;

#[derive(Debug)]
pub struct ResumeEmit(value::Value);

impl Context {
    pub fn new() -> Context {
        let isolate = v8::Isolate::new();

        Context { isolate: isolate }
    }

    pub fn process(&self, name: &str) -> error::Result<Process> {
        let context = v8::Context::new(&self.isolate);
        let global = context.global();

        let log_fun = build_log_fun(&self.isolate, &context);

        let require_fun = build_require(&self.isolate, &context);

        // TODO: convert api.js and prelude.js to modules so we can just use load_module
        let rq_native_obj = v8::value::Object::new(&self.isolate, &context);
        let log_key = v8::value::String::from_str(&self.isolate, "log");
        rq_native_obj.set(&context, &log_key, &log_fun);

        let rq_obj = v8::value::Object::new(&self.isolate, &context);
        let native_key = v8::value::String::from_str(&self.isolate, "native");
        rq_obj.set(&context, &native_key, &rq_native_obj);

        let rq_key = v8::value::String::from_str(&self.isolate, "rq");
        let require_key = v8::value::String::from_str(&self.isolate, "require");
        global.set(&context, &rq_key, &rq_obj);
        global.set(&context, &require_key, &require_fun);

        r#try!(load_embedded_file(
            &self.isolate,
            &context,
            "api.js",
            API_JS
        ));
        r#try!(load_embedded_file(
            &self.isolate,
            &context,
            "prelude.js",
            PRELUDE_JS
        ));

        let name_key = v8::value::String::from_str(&self.isolate, name);
        let generator_fn = global.get(&context, &name_key);

        if !generator_fn.is_generator_function() {
            return Err(error::Error::ProcessNotFound {
                name: name.to_owned(),
            });
        }

        let process_key = v8::value::String::from_str(&self.isolate, "Process");
        let process_fn =
            r#try!(rq_obj
                .get(&context, &process_key)
                .into_object()
                .ok_or(error::Error::Internal(
                    "The rq.Process global variable was not an object"
                )));
        let process = r#try!(process_fn.call_as_constructor(&context, &[&generator_fn]));
        let process = r#try!(process.into_object().ok_or(error::Error::Internal(
            "The constructed Process was not an object"
        )));
        let resume_key = v8::value::String::from_str(&self.isolate, "resume");
        let resume = process.get(&context, &resume_key).into_function().unwrap();

        Ok(Process {
            context: self,
            v8_context: context,
            resume: resume,
            state: State::Start,
        })
    }

    pub fn run_enqueued_tasks(&self) {
        self.isolate.run_enqueued_tasks();
    }
}

impl<'a> Process<'a> {
    pub fn context(&self) -> &Context {
        self.context
    }
}

impl State {
    pub fn resume(&mut self) -> Resume {
        match mem::replace(self, State::Pending) {
            State::Start => Resume::Start(ResumeStart),
            State::Pending => Resume::Pending(ResumePending),
            State::Await => Resume::Await(ResumeAwait),
            State::Emit(v) => Resume::Emit(ResumeEmit(v)),
            State::End => Resume::End,
        }
    }

    fn from_resume(isolate: &v8::Isolate, context: &v8::Context, resume: v8::Value) -> State {
        if resume.is_object() {
            let resume = resume.into_object().unwrap();

            let type_key = v8::value::String::from_str(isolate, "type");
            let type_value = resume
                .get(context, &type_key)
                .into_string()
                .expect("Generator resume type was not a string")
                .value();

            match type_value.as_str() {
                "await" => State::Await,
                "emit" => {
                    let value_key = v8::value::String::from_str(isolate, "value");
                    let value = resume.get(context, &value_key);
                    State::Emit(value_from_v8(isolate, context, value))
                }
                _ => panic!("Unrecognized generator type: {:?}", type_value),
            }
        } else if resume.is_undefined() {
            State::End
        } else {
            panic!(
                "Generator resumed with some unrecognized value: {}",
                resume.to_detail_string(&context).value()
            )
        }
    }
}

impl ResumeStart {
    pub fn run(self, process: &Process, args: &[query::Expression]) -> error::Result<State> {
        let isolate = &process.context.isolate;
        let context = &process.v8_context;

        let params = v8::value::Object::new(isolate, context);
        let type_key = v8::value::String::from_str(isolate, "type");
        let type_value = v8::value::String::from_str(isolate, "start");
        let args_key = v8::value::String::from_str(isolate, "args");
        let args_value = v8::value::Array::new(isolate, context, args.len() as u32);

        for (i, arg) in args.iter().enumerate() {
            args_value.set_index(context, i as u32, &r#try!(expr_to_v8(isolate, context, arg)));
        }

        params.set(context, &type_key, &type_value);
        params.set(context, &args_key, &args_value);

        r#try!(process.resume.call(context, &[&params]));

        Ok(State::Pending)
    }
}

impl ResumePending {
    pub fn run(self, process: &Process) -> error::Result<State> {
        let isolate = &process.context.isolate;
        let context = &process.v8_context;

        let params = v8::value::Object::new(isolate, context);
        let type_key = v8::value::String::from_str(isolate, "type");
        let type_value = v8::value::String::from_str(isolate, "pending");

        params.set(context, &type_key, &type_value);

        let resume = r#try!(process.resume.call(context, &[&params]));
        Ok(State::from_resume(isolate, context, resume))
    }
}

impl ResumeAwait {
    pub fn run(self, process: &Process, value: Option<value::Value>) -> error::Result<State> {
        let isolate = &process.context.isolate;
        let context = &process.v8_context;

        let params = v8::value::Object::new(isolate, context);
        let type_key = v8::value::String::from_str(isolate, "type");
        let type_value = v8::value::String::from_str(isolate, "await");
        let has_next_key = v8::value::String::from_str(isolate, "hasNext");
        let next_key = v8::value::String::from_str(isolate, "next");
        let (has_next_value, next_value) = match value {
            Some(v) => (v8::value::true_(isolate), value_to_v8(isolate, context, &v)),
            None => (
                v8::value::false_(isolate),
                v8::value::undefined(isolate).into(),
            ),
        };

        params.set(context, &type_key, &type_value);
        params.set(context, &has_next_key, &has_next_value);
        params.set(context, &next_key, &next_value);

        let resume = r#try!(process.resume.call(context, &[&params]));
        Ok(State::from_resume(isolate, context, resume))
    }
}

impl ResumeEmit {
    pub fn run(self) -> error::Result<(State, value::Value)> {
        Ok((State::Pending, self.0))
    }
}

fn build_log_fun(isolate: &v8::Isolate, outer_context: &v8::Context) -> v8::value::Function {
    let context = outer_context.clone();
    v8::value::Function::new(
        isolate,
        outer_context,
        2,
        Box::new(move |mut info| {
            let isolate = info.isolate;
            if info.args.len() < 2 {
                let message = v8::value::String::from_str(
                    &isolate,
                    "log() called with too few arguments, \
                     requires at least two",
                );
                Err(v8::value::Exception::error(&isolate, &message).into())
            } else if let (Some(level), Some(name)) = (
                info.args.remove(0).into_int32(),
                info.args.remove(0).into_string(),
            ) {
                let level = match level.value() {
                    0 => log::Level::Trace,
                    1 => log::Level::Debug,
                    2 => log::Level::Info,
                    3 => log::Level::Warn,
                    4 => log::Level::Error,
                    _ => log::Level::Error,
                };

                if log_enabled!(level) {
                    let name = name.value();

                    let args = info
                        .args
                        .iter()
                        .map(|v| v.to_string(&context).value())
                        .collect::<Vec<_>>()
                        .join(" ");

                    log!(level, "{}: {}", name, args);
                }

                Ok(v8::value::undefined(&isolate).into())
            } else {
                let message = v8::value::String::from_str(
                    &isolate,
                    &format!(
                        "log() called with bad \
                         arguments, the first two \
                         arguments should be an int and \
                         a string"
                    ),
                );
                Err(v8::value::Exception::error(&isolate, &message).into())
            }
        }),
    )
}

type ModuleCache = cell::RefCell<collections::HashMap<String, v8::value::Object>>;

fn build_require(isolate: &v8::Isolate, outer_context: &v8::Context) -> v8::value::Function {
    let context = outer_context.clone();
    let module_cache = cell::RefCell::new(collections::HashMap::new());
    v8::value::Function::new(
        isolate,
        outer_context,
        1,
        Box::new(move |mut info| {
            let isolate = info.isolate;
            if info.args.len() < 1 {
                let message =
                    v8::value::String::from_str(&isolate, "require() called without any arguments");
                Err(v8::value::Exception::error(&isolate, &message).into())
            } else if let Some(required_name) = info.args.remove(0).into_string() {
                let required_name = required_name.value();

                for &(name, source) in MODULES.iter() {
                    if name == required_name {
                        return Ok(load_module(&isolate, &context, &module_cache, name, source));
                    }
                }

                let message = v8::value::String::from_str(
                    &isolate,
                    &format!("module not found: {:?}", required_name),
                );
                Err(v8::value::Exception::error(&isolate, &message).into())
            } else {
                let message = v8::value::String::from_str(
                    &isolate,
                    "require() called with a non-string \
                     argument",
                );
                Err(v8::value::Exception::error(&isolate, &message).into())
            }
        }),
    )
}

fn load_module(
    isolate: &v8::Isolate,
    context: &v8::Context,
    module_cache: &ModuleCache,
    name: &str,
    source: &str,
) -> v8::Value {
    let exports_key = v8::value::String::from_str(isolate, "exports");

    let (module_value, should_init) = match module_cache.borrow_mut().entry(name.to_owned()) {
        collections::hash_map::Entry::Occupied(o) => {
            debug!("Re-using already loaded module {}", name);
            (o.get().clone(), false)
        }
        collections::hash_map::Entry::Vacant(v) => {
            debug!("Loading module {} for the first time", name);
            (
                v.insert(v8::value::Object::new(isolate, context)).clone(),
                true,
            )
        }
    };

    if should_init {
        let exports_value = v8::value::Object::new(isolate, context);

        let id_key = v8::value::String::from_str(isolate, "id");
        let id_value = v8::value::String::from_str(isolate, name);
        module_value.set(context, &id_key, &id_value);
        module_value.set(context, &exports_key, &exports_value);

        let source_name = v8::value::String::from_str(isolate, &format!("{}.js", name));
        let source = v8::value::String::from_str(isolate, source);
        let script =
            v8::Script::compile_with_name(isolate, context, &source_name, &source).unwrap();

        let module_key = v8::value::String::from_str(isolate, "module");

        let global = context.global();
        let old_module = global.get(context, &module_key);
        let old_exports = global.get(context, &exports_key);

        global.set(context, &module_key, &module_value);
        global.set(context, &exports_key, &exports_value);

        script.run(context).unwrap();

        global.set(context, &module_key, &old_module);
        global.set(context, &exports_key, &old_exports);
    }

    module_value.get(context, &exports_key)
}

fn load_embedded_file(
    isolate: &v8::Isolate,
    context: &v8::Context,
    file_name: &str,
    file_contents: &str,
) -> error::Result<()> {
    let file_name = v8::value::String::from_str(isolate, file_name);
    let file_contents = v8::value::String::from_str(isolate, file_contents);
    let script = r#try!(v8::Script::compile_with_name(
        isolate,
        context,
        &file_name,
        &file_contents
    ));
    r#try!(script.run(context));
    Ok(())
}

fn expr_to_v8(
    isolate: &v8::Isolate,
    context: &v8::Context,
    expr: &query::Expression,
) -> error::Result<v8::Value> {
    match *expr {
        query::Expression::Value(ref v) => Ok(value_to_v8(isolate, context, v)),
        query::Expression::Function(ref args, ref body) => {
            let function_key = v8::value::String::from_str(isolate, "Function");
            let mut args = args
                .iter()
                .map(|a| v8::value::String::from_str(isolate, a).into())
                .collect::<Vec<v8::Value>>();
            args.push(v8::value::String::from_str(isolate, &format!("return {};", body)).into());

            let arg_refs = args.iter().collect::<Vec<&v8::Value>>();

            let function_ctor = context
                .global()
                .get(context, &function_key)
                .into_object()
                .unwrap();
            let function = r#try!(function_ctor.call_as_constructor(context, &arg_refs));
            Ok(function)
        }
        query::Expression::Javascript(ref v) => {
            let source = v8::value::String::from_str(isolate, v);
            let script = r#try!(v8::Script::compile(isolate, context, &source));
            let result = r#try!(script.run(context));
            Ok(result)
        }
    }
}

fn value_to_v8(isolate: &v8::Isolate, context: &v8::Context, value: &value::Value) -> v8::Value {
    use crate::value::Value::*;
    match *value {
        Unit => v8::value::null(isolate).into(),
        Bool(v) => v8::value::Boolean::new(isolate, v).into(),

        I8(v) => v8::value::Integer::new(isolate, v as i32).into(),
        I16(v) => v8::value::Integer::new(isolate, v as i32).into(),
        I32(v) => v8::value::Integer::new(isolate, v).into(),
        I64(v) => v8::value::Number::new(isolate, v as f64).into(),

        U8(v) => v8::value::Integer::new_from_unsigned(isolate, v as u32).into(),
        U16(v) => v8::value::Integer::new_from_unsigned(isolate, v as u32).into(),
        U32(v) => v8::value::Integer::new_from_unsigned(isolate, v).into(),
        U64(v) => v8::value::Number::new(isolate, v as f64).into(),

        F32(ordered_float::OrderedFloat(v)) => v8::value::Number::new(isolate, v as f64).into(),
        F64(ordered_float::OrderedFloat(v)) => v8::value::Number::new(isolate, v).into(),

        Char(v) => v8::value::String::from_str(isolate, &format!("{}", v)).into(),
        String(ref v) => v8::value::String::from_str(isolate, v.as_str()).into(),
        Bytes(ref v) => {
            // TODO: this is a stop gap until there is a stable API for byte buffers in V8...
            let hex = v
                .iter()
                .map(|b| format!("{:02x}", b))
                .collect::<Vec<_>>()
                .join("");
            v8::value::String::from_str(isolate, &hex).into()
        }

        Sequence(ref v) => {
            let a = v8::value::Array::new(isolate, context, v.len() as u32);

            for (i, e) in v.into_iter().enumerate() {
                a.set_index(context, i as u32, &value_to_v8(isolate, context, &e));
            }

            a.into()
        }
        Map(ref m) => {
            let o = v8::value::Object::new(isolate, context);

            for (k, v) in m.into_iter() {
                let key = value_to_v8(isolate, context, &k);
                let value = value_to_v8(isolate, context, &v);
                o.set(context, &key, &value);
            }

            o.into()
        }
    }
}

fn value_from_v8(isolate: &v8::Isolate, context: &v8::Context, value: v8::Value) -> value::Value {
    if value.is_null() || value.is_undefined() {
        value::Value::Unit
    } else if value.is_true() {
        value::Value::Bool(true)
    } else if value.is_false() {
        value::Value::Bool(false)
    } else if value.is_number() {
        let v = value.into_number().unwrap().value();
        if (v as u8) as f64 == v {
            value::Value::U8(v as u8)
        } else if (v as u16) as f64 == v {
            value::Value::U16(v as u16)
        } else if (v as u32) as f64 == v {
            value::Value::U32(v as u32)
        } else if (v as u64) as f64 == v {
            value::Value::U64(v as u64)
        } else if (v as i8) as f64 == v {
            value::Value::I8(v as i8)
        } else if (v as i16) as f64 == v {
            value::Value::I16(v as i16)
        } else if (v as i32) as f64 == v {
            value::Value::I32(v as i32)
        } else if (v as i64) as f64 == v {
            value::Value::I64(v as i64)
        } else if (v as f32) as f64 == v {
            value::Value::from_f32(v as f32)
        } else {
            value::Value::from_f64(v)
        }
    } else if value.is_string() {
        value::Value::String(value.into_string().unwrap().value())
    } else if value.is_array_buffer() {
        unimplemented!()
    } else if value.is_array() {
        let array = value.into_array().unwrap();
        let length_key = v8::value::String::from_str(isolate, "length");
        let length = array
            .get(context, &length_key)
            .into_uint32()
            .unwrap()
            .value();

        let mut result = Vec::with_capacity(length as usize);

        for i in 0..length {
            result.push(value_from_v8(isolate, context, array.get_index(context, i)));
        }

        value::Value::Sequence(result)
    } else if value.is_object() {
        let object = value.into_object().unwrap();

        let keys = object.get_own_property_names(context);
        let length_key = v8::value::String::from_str(isolate, "length");
        let keys_length = keys
            .get(context, &length_key)
            .into_uint32()
            .unwrap()
            .value();

        let mut result = collections::BTreeMap::new();
        for i in 0..keys_length {
            let key = keys.get_index(context, i).to_detail_string(context).into();
            let value = object.get(context, &key);
            result.insert(
                value_from_v8(isolate, context, key),
                value_from_v8(isolate, context, value),
            );
        }

        value::Value::Map(result)
    } else {
        value::Value::String(value.to_detail_string(context).value())
    }
}
