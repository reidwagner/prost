#![doc(html_root_url = "https://docs.rs/prost-codegen/0.1.1")]
#![recursion_limit = "128"]

#[macro_use]
extern crate prost_derive;
#[macro_use]
extern crate log;

extern crate bytes;
extern crate env_logger;
extern crate itertools;
extern crate multimap;
extern crate petgraph;
extern crate prost;

mod message_graph;

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use itertools::{Either, Itertools};
use multimap::MultiMap;

mod ast;
pub mod google;
use google::protobuf::{
    DescriptorProto,
    EnumDescriptorProto,
    EnumValueDescriptorProto,
    field_descriptor_proto,
    FieldDescriptorProto,
    FileDescriptorProto,
    OneofDescriptorProto,
    ServiceDescriptorProto,
    SourceCodeInfo,
};
use google::protobuf::source_code_info::Location;

use message_graph::MessageGraph;
pub use ast::{
    Comments,
    Method,
    Service,
};

pub fn module(file: &FileDescriptorProto) -> Module {
    file.package()
        .split('.')
        .filter(|s| !s.is_empty())
        .map(camel_to_snake)
        .collect()
}

pub type Module = Vec<String>;

pub trait ServiceGenerator {
    fn generate(&self, service: Service, buf: &mut String);
}

pub fn generate(files: Vec<FileDescriptorProto>,
                service_generator: Option<&ServiceGenerator>) -> HashMap<Module, String> {
    let mut modules = HashMap::new();

    let message_graph = MessageGraph::new(&files);

    for file in files {
        let module = module(&file);
        let mut buf = modules.entry(module).or_insert(String::new());
        CodeGenerator::generate(&service_generator, file, &message_graph, &mut buf);
    }
    modules
}

#[derive(PartialEq)]
enum Syntax {
    Proto2,
    Proto3,
}

struct CodeGenerator<'a> {
    package: String,
    source_info: SourceCodeInfo,
    syntax: Syntax,
    message_graph: &'a MessageGraph,
    depth: u8,
    path: Vec<i32>,
    buf: &'a mut String,
}

impl <'a> CodeGenerator<'a> {
    fn generate(service_generator: &Option<&ServiceGenerator>,
                file: FileDescriptorProto,
                message_graph: &MessageGraph,
                buf: &mut String) {

        let mut source_info = file.source_code_info.expect("no source code info in request");
        source_info.location.retain(|location| {
            let len = location.path.len();
            len > 0 && len % 2 == 0
        });
        source_info.location.sort_by_key(|location| location.path.clone());

        let syntax = match file.syntax.as_ref().map(String::as_str) {
            None | Some("proto2") => Syntax::Proto2,
            Some("proto3") => Syntax::Proto3,
            Some(s) => panic!("unknown syntax: {}", s),
        };

        let mut code_gen = CodeGenerator {
            package: file.package.unwrap(),
            source_info: source_info,
            syntax: syntax,
            message_graph: message_graph,
            depth: 0,
            path: Vec::new(),
            buf: buf,
        };

        debug!("file: {:?}, package: {:?}", file.name.as_ref().unwrap(), code_gen.package);

        code_gen.path.push(4);
        for (idx, message) in file.message_type.into_iter().enumerate() {
            code_gen.path.push(idx as i32);
            code_gen.append_message(message);
            code_gen.path.pop();
        }
        code_gen.path.pop();

        code_gen.path.push(5);
        for (idx, desc) in file.enum_type.into_iter().enumerate() {
            code_gen.path.push(idx as i32);
            code_gen.append_enum(desc);
            code_gen.path.pop();
        }
        code_gen.path.pop();

        if let &Some(ref service_generator) = service_generator {
            code_gen.path.push(6);
            for (idx, service) in file.service.into_iter().enumerate() {
                code_gen.path.push(idx as i32);
                service_generator.generate(code_gen.unpack_service(service), &mut code_gen.buf);
                code_gen.path.pop();
            }
            code_gen.path.pop();
        }
    }

    fn append_message(&mut self, message: DescriptorProto) {
        debug!("\tmessage: {:?}", message.name());

        // Split the nested message types into a vector of normal nested message types, and a map
        // of the map field entry types. The path index of the nested message types is preserved so
        // that comments can be retrieved.
        let message_name = message.name.as_ref().expect("message name");
        let fq_message_name = format!(".{}.{}", self.package, message_name);
        let (nested_types, map_types): (Vec<(DescriptorProto, usize)>, HashMap<String, (FieldDescriptorProto, FieldDescriptorProto)>) =
            message.nested_type.into_iter().enumerate().partition_map(|(idx, nested_type)| {
                if nested_type.options.as_ref().and_then(|options| options.map_entry).unwrap_or(false) {
                    let key = nested_type.field[0].clone();
                    let value = nested_type.field[1].clone();
                    assert_eq!("key", key.name.as_ref().expect("key name"));
                    assert_eq!("value", value.name.as_ref().expect("value name"));

                    let name = format!("{}.{}",
                                       fq_message_name,
                                       nested_type.name.as_ref().expect("nested type name"));
                    Either::Right((name, (key, value)))
                } else {
                    Either::Left((nested_type, idx))
                }
        });

        // Split the fields into a vector of the normal fields, and oneof fields.
        // Path indexes are preserved so that comments can be retrieved.
        let (fields, mut oneof_fields): (Vec<(FieldDescriptorProto, usize)>, MultiMap<i32, (FieldDescriptorProto, usize)>) =
            message.field.into_iter().enumerate().partition_map(|(idx, field)| {
                if let Some(oneof_index) = field.oneof_index {
                    Either::Right((oneof_index, (field, idx)))
                } else {
                    Either::Left((field, idx))
                }
            });

        assert_eq!(oneof_fields.len(), message.oneof_decl.len());

        self.append_doc();
        self.push_indent();
        self.buf.push_str("#[derive(Clone, Debug, PartialEq, Message)]\n");
        self.push_indent();
        self.buf.push_str("pub struct ");
        self.buf.push_str(&message_name);
        self.buf.push_str(" {\n");

        self.depth += 1;
        self.path.push(2);
        for (field, idx) in fields.into_iter() {
            self.path.push(idx as i32);
            match field.type_name.as_ref().and_then(|type_name| map_types.get(type_name)) {
                Some(&(ref key, ref value)) => self.append_map_field(field, key, value),
                None => self.append_field(&fq_message_name, field),
            }
            self.path.pop();
        }
        self.path.pop();

        self.path.push(8);
        for (idx, oneof) in message.oneof_decl.iter().enumerate() {
            let idx = idx as i32;
            self.path.push(idx);
            self.append_oneof_field(&message_name, oneof, &oneof_fields.get_vec(&idx).unwrap());
            self.path.pop();
        }
        self.path.pop();

        self.depth -= 1;
        self.push_indent();
        self.buf.push_str("}\n");

        if !message.enum_type.is_empty() || !nested_types.is_empty() || !oneof_fields.is_empty() {
            self.push_mod(&message_name);
            self.path.push(3);
            for (nested_type, idx) in nested_types.into_iter() {
                self.path.push(idx as i32);
                self.append_message(nested_type);
                self.path.pop();
            }
            self.path.pop();

            self.path.push(4);
            for (idx, nested_enum) in message.enum_type.into_iter().enumerate() {
                self.path.push(idx as i32);
                self.append_enum(nested_enum);
                self.path.pop();
            }
            self.path.pop();

            for (idx, oneof) in message.oneof_decl.into_iter().enumerate() {
                let idx = idx as i32;
                self.append_oneof(oneof, idx, oneof_fields.remove(&idx).unwrap());
            }

            self.pop_mod();
        }
    }

    fn append_field(&mut self, msg_name: &str, field: FieldDescriptorProto) {
        use field_descriptor_proto::Label::*;

        let repeated = field.label == Some(LabelRepeated as i32);
        let optional = self.optional(&field);
        let ty = self.resolve_type(&field);

        let boxed = !repeated
                 && field.type_().unwrap() == field_descriptor_proto::Type::TypeMessage
                 && self.message_graph.is_nested(field.type_name(), msg_name);

        debug!("\t\tfield: {:?}, type: {:?}", field.name(), ty);

        self.append_doc();
        self.push_indent();
        self.buf.push_str("#[prost(");
        let type_tag = self.field_type_tag(&field);
        self.buf.push_str(&type_tag);

        match field.label().expect("unknown label") {
            LabelOptional => if optional {
                self.buf.push_str(", optional");
            },
            LabelRequired => self.buf.push_str(", required"),
            LabelRepeated => {
                self.buf.push_str(", repeated");
                if can_pack(&field) && !field.options.as_ref().map_or(false, |options| options.packed()) {
                    self.buf.push_str(", packed=\"false\"");
                }
            },
        }

        if boxed { self.buf.push_str(", boxed"); }
        self.buf.push_str(", tag=\"");
        self.buf.push_str(&field.number().to_string());
        self.buf.push_str("\")]\n");
        self.push_indent();
        self.buf.push_str("pub ");
        self.buf.push_str(&camel_to_snake(field.name()));
        self.buf.push_str(": ");
        if repeated { self.buf.push_str("Vec<"); }
        else if optional { self.buf.push_str("Option<"); }
        if boxed { self.buf.push_str("Box<"); }
        self.buf.push_str(&ty);
        if boxed { self.buf.push_str(">"); }
        if repeated || optional { self.buf.push_str(">"); }
        self.buf.push_str(",\n");
    }

    fn append_map_field(&mut self,
                        field: FieldDescriptorProto,
                        key: &FieldDescriptorProto,
                        value: &FieldDescriptorProto) {
        let key_ty = self.resolve_type(key);
        let value_ty = self.resolve_type(value);

        debug!("\t\tmap field: {:?}, key type: {:?}, value type: {:?}",
               field.name(), key_ty, value_ty);

        self.append_doc();
        self.push_indent();

        let key_tag = self.field_type_tag(key);
        let value_tag = self.map_value_type_tag(value);
        self.buf.push_str(&format!("#[prost(map=\"{}, {}\", tag=\"{}\")]\n",
                                   key_tag,
                                   value_tag,
                                   field.number()));
        self.push_indent();
        self.buf.push_str(&format!("pub {}: ::std::collections::HashMap<{}, {}>,\n",
                                   camel_to_snake(field.name()), key_ty, value_ty));
    }

    fn append_oneof_field(&mut self,
                          message_name: &str,
                          oneof: &OneofDescriptorProto,
                          fields: &[(FieldDescriptorProto, usize)]) {
        let name = format!("{}::{}",
                           camel_to_snake(message_name),
                           snake_to_upper_camel(oneof.name()));
        self.append_doc();
        self.push_indent();
        self.buf.push_str(&format!("#[prost(oneof=\"{}\", tags=\"{}\")]\n",
                                   name,
                                   fields.iter().map(|&(ref field, _)| field.number()).join(", ")));
        self.push_indent();
        self.buf.push_str(&format!("pub {}: Option<{}>,\n", camel_to_snake(oneof.name()), name));
    }

    fn append_oneof(&mut self,
                    oneof: OneofDescriptorProto,
                    idx: i32,
                    fields: Vec<(FieldDescriptorProto, usize)>) {
        self.path.push(8);
        self.path.push(idx);
        self.append_doc();
        self.path.pop();
        self.path.pop();

        self.push_indent();
        self.buf.push_str("#[derive(Clone, Debug, Oneof, PartialEq)]\n");
        self.push_indent();
        self.buf.push_str("pub enum ");
        self.buf.push_str(&snake_to_upper_camel(oneof.name()));
        self.buf.push_str(" {\n");

        self.path.push(2);
        self.depth += 1;
        for (field, idx) in fields {
            self.path.push(idx as i32);
            self.append_doc();
            self.path.pop();

            self.push_indent();
            let ty_tag = self.field_type_tag(&field);
            self.buf.push_str(&format!("#[prost({}, tag=\"{}\")]\n", ty_tag, field.number()));

            self.push_indent();
            let ty = self.resolve_type(&field);
            self.buf.push_str(&format!("{}({}),\n", snake_to_upper_camel(field.name()), ty));
        }
        self.depth -= 1;
        self.path.pop();

        self.push_indent();
        self.buf.push_str("}\n");
    }

    fn location(&self) -> &Location {
        let idx = self.source_info
                      .location
                      .binary_search_by_key(&&self.path[..], |location| &location.path[..])
                      .unwrap();

        &self.source_info.location[idx]
    }

    fn append_doc(&mut self) {
        let idx = self.source_info
                      .location
                      .binary_search_by_key(&&self.path[..], |location| &location.path[..])
                      .unwrap();

        let location = &self.source_info.location[idx];

        for comment in &location.leading_detached_comments {
            for line in comment.lines() {
                for _ in 0..self.depth {
                    self.buf.push_str("    ");
                }
                self.buf.push_str("//");
                self.buf.push_str(line);
                self.buf.push_str("\n");
            }
            self.buf.push_str("\n");
        }

        if let Some(ref comments) = location.leading_comments {
            for line in comments.lines() {
                for _ in 0..self.depth {
                    self.buf.push_str("    ");
                }
                self.buf.push_str("///");
                self.buf.push_str(line);
                self.buf.push_str("\n");
            }
        }
        if let Some(ref comments) = location.trailing_comments {
            for line in comments.lines() {
                for _ in 0..self.depth {
                    self.buf.push_str("    ");
                }
                self.buf.push_str("///");
                self.buf.push_str(line);
                self.buf.push_str("\n");
            }
        }
    }

    fn append_enum(&mut self, desc: EnumDescriptorProto) {
        debug!("\tenum: {:?}", desc.name());

        self.append_doc();
        self.push_indent();
        self.buf.push_str("#[derive(Clone, Copy, Debug, PartialEq, Eq, Enumeration)]\n");
        self.push_indent();
        self.buf.push_str("pub enum ");
        self.buf.push_str(desc.name());
        self.buf.push_str(" {\n");

        let mut numbers = HashSet::new();

        self.depth += 1;
        self.path.push(2);
        for (idx, value) in desc.value.into_iter().enumerate() {
            // Skip duplicate enum values. Protobuf allows this when the
            // 'allow_alias' option is set.
            if !numbers.insert(value.number()) {
                continue;
            }

            self.path.push(idx as i32);
            self.append_enum_value(value);
            self.path.pop();
        }
        self.path.pop();
        self.depth -= 1;

        self.push_indent();
        self.buf.push_str("}\n");
    }

    fn append_enum_value(&mut self, value: EnumValueDescriptorProto) {
        self.append_doc();
        self.push_indent();
        self.buf.push_str(&snake_to_upper_camel(value.name()));
        self.buf.push_str(" = ");
        self.buf.push_str(&value.number().to_string());
        self.buf.push_str(",\n");
    }

    fn unpack_service(&mut self, service: ServiceDescriptorProto) -> Service {
        let name = service.name().to_owned();
        debug!("\t service: {:?}", name);

        let comments = Comments::from_location(self.location());

        let methods = service.method
                              .into_iter()
                              .enumerate()
                              .map(|(idx, mut method)| {
                                  self.path.push(idx as i32);
                                  let comments = Comments::from_location(self.location());
                                  self.path.pop();

                                  let name = method.name.take().unwrap();
                                  let input_proto_type = method.input_type.take().unwrap();
                                  let output_proto_type = method.output_type.take().unwrap();
                                  let input_type = self.resolve_ident(&input_proto_type);
                                  let output_type = self.resolve_ident(&output_proto_type);

                                  Method {
                                      name,
                                      comments,
                                      input_type,
                                      input_proto_type,
                                      output_type,
                                      output_proto_type
                                  }
                              })
                              .collect();

        Service {
            name,
            comments,
            methods
        }
    }

    fn push_indent(&mut self) {
        for _ in 0..self.depth {
            self.buf.push_str("    ");
        }
    }

    fn push_mod(&mut self, module: &str) {
        self.push_indent();
        self.buf.push_str("pub mod ");
        self.buf.push_str(&camel_to_snake(module));
        self.buf.push_str(" {\n");

        self.package.push_str(".");
        self.package.push_str(module);

        self.depth += 1;
    }

    fn pop_mod(&mut self) {
        self.depth -= 1;

        let idx = self.package.rfind('.').unwrap();
        self.package.truncate(idx);

        self.push_indent();
        self.buf.push_str("}\n");
    }

    fn resolve_type<'b>(&self, field: &'b FieldDescriptorProto) -> Cow<'b, str> {
        use field_descriptor_proto::Type::*;
        match field.type_().expect("unknown field type") {
            TypeFloat => Cow::Borrowed("f32"),
            TypeDouble => Cow::Borrowed("f64"),
            TypeUint32 | TypeFixed32 => Cow::Borrowed("u32"),
            TypeUint64 | TypeFixed64 => Cow::Borrowed("u64"),
            TypeInt32 | TypeSfixed32 | TypeSint32 => Cow::Borrowed("i32"),
            TypeInt64 | TypeSfixed64 | TypeSint64 => Cow::Borrowed("i64"),
            TypeBool => Cow::Borrowed("bool"),
            TypeString => Cow::Borrowed("String"),
            TypeBytes => Cow::Borrowed("Vec<u8>"),
            TypeGroup | TypeMessage => Cow::Owned(self.resolve_ident(field.type_name())),
            TypeEnum => Cow::Borrowed("i32"),
        }
    }

    fn resolve_ident(&self, pb_ident: &str) -> String {
        // protoc should always give fully qualified identifiers.
        assert_eq!(".", &pb_ident[..1]);

        let mut local_path = self.package.split('.').peekable();

        let mut ident_path = pb_ident[1..].split('.');
        let ident_type = ident_path.next_back().unwrap();
        let mut ident_path = ident_path.peekable();

        // Skip path elements in common.
        while local_path.peek().is_some() &&
              local_path.peek() == ident_path.peek() {
            local_path.next();
            ident_path.next();
        }

        local_path.map(|_| "super".to_string())
                  .chain(ident_path.map(camel_to_snake))
                  .chain(Some(ident_type.to_string()).into_iter())
                  .join("::")
    }

    fn field_type_tag(&self, field: &FieldDescriptorProto) -> Cow<'static, str> {
        use field_descriptor_proto::Type::*;
        match field.type_().expect("unknown field type") {
            TypeFloat => Cow::Borrowed("float"),
            TypeDouble => Cow::Borrowed("double"),
            TypeInt32 => Cow::Borrowed("int32"),
            TypeInt64 => Cow::Borrowed("int64"),
            TypeUint32 => Cow::Borrowed("uint32"),
            TypeUint64 => Cow::Borrowed("uint64"),
            TypeSint32 => Cow::Borrowed("sint32"),
            TypeSint64 => Cow::Borrowed("sint64"),
            TypeFixed32 => Cow::Borrowed("fixed32"),
            TypeFixed64 => Cow::Borrowed("fixed64"),
            TypeSfixed32 => Cow::Borrowed("sfixed32"),
            TypeSfixed64 => Cow::Borrowed("sfixed64"),
            TypeBool => Cow::Borrowed("bool"),
            TypeString => Cow::Borrowed("string"),
            TypeBytes => Cow::Borrowed("bytes"),
            TypeGroup => Cow::Borrowed("group"),
            TypeMessage => Cow::Borrowed("message"),
            TypeEnum => Cow::Owned(format!("enumeration={:?}", self.resolve_ident(field.type_name()))),
        }
    }

    fn map_value_type_tag(&self, field: &FieldDescriptorProto) -> Cow<'static, str> {
        use field_descriptor_proto::Type::*;
        match field.type_().expect("unknown field type") {
            TypeEnum => Cow::Owned(format!("enumeration({})", self.resolve_ident(field.type_name()))),
            _ => self.field_type_tag(field),
        }
    }

    fn optional(&self, field: &FieldDescriptorProto) -> bool {
        if field.label().expect("unknown label") != field_descriptor_proto::Label::LabelOptional {
            return false;
        }

        use field_descriptor_proto::Type::*;
        match field.type_().expect("unknown field type") {
            TypeMessage => true,
            _ => self.syntax == Syntax::Proto2,
        }
    }
}

fn can_pack(field: &FieldDescriptorProto) -> bool {
        use field_descriptor_proto::Type::*;
        match field.type_().expect("unknown field type") {
            TypeFloat  | TypeDouble | TypeInt32   | TypeInt64   | TypeUint32   | TypeUint64   |
            TypeSint32 | TypeSint64 | TypeFixed32 | TypeFixed64 | TypeSfixed32 | TypeSfixed64 |
            TypeBool | TypeEnum => true,
            _ => false,
        }
}

fn camel_to_snake(camel: &str) -> String {
    // protoc does not allow non-ascii identifiers.
    let len = camel.as_bytes().iter().skip(1).filter(|&&c| is_uppercase(c)).count() + camel.len();
    let mut snake = Vec::with_capacity(len);

    let mut break_on_cap = false;
    for &c in camel.as_bytes().iter() {
        if is_uppercase(c) {
            if break_on_cap {
                snake.push(b'_');
            }
            snake.push(to_lowercase(c));
            break_on_cap = false;
        } else if c == b'_' {
            snake.push(b'_');
            break_on_cap = false;
        } else {
            snake.push(c);
            break_on_cap = true;
        }
    }

    let mut ident = String::from_utf8(snake).expect(&format!("non-utf8 identifier: {}", camel));

    // Add a trailing underscore if the identifier matches a Rust keyword
    // (https://doc.rust-lang.org/grammar.html#keywords).
    match &ident[..] {
        "abstract" | "alignof" | "as"     | "become"  | "box"   | "break"   | "const"    |
        "continue" | "crate"   | "do"     | "else"    | "enum"  | "extern"  | "false"    |
        "final"    | "fn"      | "for"    | "if"      | "impl"  | "in"      | "let"      |
        "loop"     | "macro"   | "match"  | "mod"     | "move"  | "mut"     | "offsetof" |
        "override" | "priv"    | "proc"   | "pub"     | "pure"  | "ref"     | "return"   |
        "self"     | "sizeof"  | "static" | "struct"  | "super" | "trait"   | "true"     |
        "type"     | "typeof"  | "unsafe" | "unsized" | "use"   | "virtual" | "where"    |
        "while"    | "yield" => {
            ident.push('_');
        }
        _ => (),
    }
    ident
}

fn snake_to_upper_camel(snake: &str) -> String {
    let mut s = String::with_capacity(snake.len());

    if snake.is_empty() {
        return s;
    }

    for fragment in snake.split('_') {
        if fragment.is_empty() {
            s.push('_');
        } else {
            let (first, rest) = fragment.split_at(1);
            s.push_str(&first.to_uppercase());
            s.push_str(&rest.to_lowercase());
        }
    }
    s
}

#[inline]
fn is_uppercase(c: u8) -> bool {
    c >= b'A' && c <= b'Z'
}

#[inline]
fn to_lowercase(c: u8) -> u8 {
    debug_assert!(is_uppercase(c));
    c + 32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_camel_to_snake() {
        assert_eq!("foo_bar", &camel_to_snake("FooBar"));
        assert_eq!("foo_bar_baz", &camel_to_snake("FooBarBAZ"));
        assert_eq!("foo_bar_baz", &camel_to_snake("FooBArBAZ"));
        assert_eq!("foo_bar_bazle_e", &camel_to_snake("FooBArBAZleE"));
        assert_eq!("while_", &camel_to_snake("While"));
        assert_eq!("fuzz_buster", &camel_to_snake("FUZZ_BUSTER"));
        assert_eq!("foo_bar_baz", &camel_to_snake("foo_bar_baz"));
        assert_eq!("fuzz_buster", &camel_to_snake("FUZZ_buster"));
        assert_eq!("_fuzz", &camel_to_snake("_FUZZ"));
        assert_eq!("_fuzz", &camel_to_snake("_fuzz"));
        assert_eq!("_fuzz", &camel_to_snake("_Fuzz"));
        assert_eq!("fuzz_", &camel_to_snake("FUZZ_"));
        assert_eq!("fuzz_", &camel_to_snake("fuzz_"));
        assert_eq!("fuzz_", &camel_to_snake("Fuzz_"));
        assert_eq!("fuz_z_", &camel_to_snake("FuzZ_"));


        // From test_messages_proto3.proto.
        assert_eq!("fieldname1", &camel_to_snake("fieldname1"));
        assert_eq!("field_name2", &camel_to_snake("field_name2"));
        assert_eq!("_field_name3", &camel_to_snake("_field_name3"));
        assert_eq!("field__name4_", &camel_to_snake("field__name4_"));
        assert_eq!("field0name5", &camel_to_snake("field0name5"));
        assert_eq!("field_0_name6", &camel_to_snake("field_0_name6"));
        assert_eq!("field_name7", &camel_to_snake("fieldName7"));
        assert_eq!("field_name8", &camel_to_snake("FieldName8"));
        assert_eq!("field_name9", &camel_to_snake("field_Name9"));
        assert_eq!("field_name10", &camel_to_snake("Field_Name10"));
        assert_eq!("field_name11", &camel_to_snake("FIELD_NAME11"));
        assert_eq!("field_name12", &camel_to_snake("FIELD_name12"));
        assert_eq!("__field_name13", &camel_to_snake("__field_name13"));
        assert_eq!("__field_name14", &camel_to_snake("__Field_name14"));
        assert_eq!("field__name15", &camel_to_snake("field__name15"));
        assert_eq!("field__name16", &camel_to_snake("field__Name16"));
        assert_eq!("field_name17__", &camel_to_snake("field_name17__"));
        assert_eq!("field_name18__", &camel_to_snake("Field_name18__"));
    }

    #[test]
    fn test_snake_to_upper_camel() {
        assert_eq!("", &snake_to_upper_camel(""));
        assert_eq!("F", &snake_to_upper_camel("F"));
        assert_eq!("Foo", &snake_to_upper_camel("FOO"));
        assert_eq!("FooBar", &snake_to_upper_camel("FOO_BAR"));
        assert_eq!("_FooBar", &snake_to_upper_camel("_FOO_BAR"));
        assert_eq!("FooBar_", &snake_to_upper_camel("FOO_BAR_"));
        assert_eq!("_FooBar_", &snake_to_upper_camel("_FOO_BAR_"));
        assert_eq!("Fuzzbuster", &snake_to_upper_camel("fuzzBuster"));
    }
}
