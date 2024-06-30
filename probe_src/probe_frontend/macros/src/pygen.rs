use std::fmt::Display;
use std::fs::File;
use std::io::Write;
use std::sync::{OnceLock, RwLock};
use syn::{Data, Fields};

/// statically defined python code that gets added to the begining of the outputed file
const PYGEN_PREAMBLE: &str = "
# This file is automatically @generated by probe_macros

import sys
import typing
from dataclasses import dataclass

mod = sys.modules[__name__]

";

pub fn make_py_dataclass_internal(input: syn::DeriveInput) {
    let syn::DeriveInput { data, ident, .. } = input.clone();

    match data {
        Data::Struct(data_struct) => {
            let fields = match data_struct.fields {
                Fields::Named(x) => x,
                _ => unimplemented!("unnamed and unit structs not implemented"),
            };

            let pairs = fields
                .named
                .iter()
                .map(|x| {
                    let ident = x.ident.as_ref().unwrap();
                    (ident.to_string(), convert_to_pytype(&x.ty))
                })
                .collect::<Vec<(_, _)>>();

            write_pygen(basic_dataclass(ident.to_string(), &pairs));
        }
        Data::Enum(data_enum) => {
            // let mut dataclass = format!("@dataclass(init=False)\nclass {}:\n", ident);
            let mut dataclass = Dataclass::new(ident.to_string());
            let mut init = DataclassInit::new();
            let mut args = InitArgs::new();

            // this is the types that the produced union is over
            let mut variants = vec![];

            for variant in data_enum.variants {
                match variant.fields {
                    syn::Fields::Named(inner) => {
                        let name = variant.ident.to_string();

                        let pairs = inner
                            .named
                            .iter()
                            .map(|x| {
                                let name = x.ident.as_ref().unwrap();
                                (name.to_string(), convert_to_pytype(&x.ty))
                            })
                            .collect::<Vec<_>>();

                        dataclass.add_inclass(basic_dataclass(name.clone(), &pairs));
                        variants.push(name);
                    }
                    syn::Fields::Unnamed(inner) => {
                        let fields = inner.unnamed.iter().collect::<Vec<_>>();
                        if fields.len() != 1 {
                            unimplemented!("Tuple enums of length != 1 not supported")
                        }
                        variants.push(convert_to_pytype(&fields[0].ty));
                    }
                    syn::Fields::Unit => unimplemented!("Unit enum variants not supported"),
                }
            }

            // here we merge the variants together in a python union
            let union_type = format!(
                "typing.Union[{}]",
                variants
                    .iter()
                    .fold(String::new(), |mut acc, x| {
                        acc.push_str(x);
                        acc.push_str(", ");

                        acc
                    })
                    .strip_suffix(", ")
                    .expect("union had no variants")
            );
            dataclass.add_item(DataclassItem::new("value".to_owned(), union_type));

            args.add(
                "**kwargs".to_owned(),
                "typing.Mapping[str, typing.Any]".to_owned(),
            );
            // add custom init that does some quasi-quoting hackery
            [
                "if len(kwargs) != 1:",
                "    raise ValueError(\"Malformed Enum constructor args\")",
                "key = list(kwargs.keys())[0]",
                "if key in self.__class__.__dict__:",
                "    self.value = self.__class__.__dict__[key](**kwargs[key])",
                "else:",
                "    self.value = mod.__dict__[key](**kwargs[key])",
            ]
            .into_iter()
            .for_each(|line| init.add_line(line.to_owned()));

            init.set_args(args);
            dataclass.set_init(Some(init));
            write_pygen(dataclass);
        }
        Data::Union(_data_union) => unimplemented!(),
    };
}

fn basic_dataclass(name: String, pairs: &[(String, String)]) -> Dataclass {
    // this function take a type and identifier that's part of the argumetns to the init fucnction
    // and creates the expression for converting it for sotrage in the dataclass, basically this
    // means running primitive types through their type constructor to validate them and for other
    // dataclasses the arg get unpacked and passed to the relevant class constructor.
    fn make_conversion(ident: &str, ty: &str) -> String {
        match ty {
            // don't unpack primitive types
            "bytes" | "int" | "str" | "bool" => format!("{}({})", ty, ident),
            _ => format!("{}(**{})", ty, ident),
        }
    }

    let mut dataclass = Dataclass::new(name);
    let mut init = DataclassInit::new();
    let mut args = InitArgs::new();

    for (ident, ty) in pairs {
        dataclass.add_item(DataclassItem::new(ident.clone(), ty.clone()));
        init.add_line(format!("self.{} = {}", ident, make_conversion(ident, ty)));
        args.add(ident.clone(), ty.clone());
    }

    init.set_args(args);
    dataclass.set_init(Some(init));

    dataclass
}

fn convert_to_pytype(ty: &syn::Type) -> String {
    match ty {
        syn::Type::Array(inner) => {
            format!("list[{}]", convert_to_pytype(inner.elem.as_ref()))
        }
        syn::Type::Path(inner) => {
            let name = crate::type_basename(inner).to_string();
            match name.as_str() {
                "__dev_t" | "__gid_t" | "__ino_t" | "__mode_t" | "__s32" | "__s64"
                | "__suseconds_t" | "__syscall_slong_t" | "__syseconds_t" | "__time_t"
                | "__u16" | "__u32" | "__u64" | "__uid_t" | "c_int" | "c_long" | "c_uint"
                | "dev_t" | "gid_t" | "i32" | "ino_t" | "mode_t" | "pid_t" | "uid_t" => {
                    "int".to_owned()
                }

                "CString" => "bytes".to_owned(),

                _ => name,
            }
        }
        _ => unimplemented!("unsupported type type"),
    }
}

fn write_pygen(item: impl Display) {
    static DATACLASSES: OnceLock<RwLock<File>> = OnceLock::new();
    let mut writer = DATACLASSES
        .get_or_init(|| {
            let mut file = File::create(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../python/generated/ops.py"
            ))
            .expect("unable to create ops.py");
            file.write_all(PYGEN_PREAMBLE.as_bytes())
                .expect("failed to write preamble");
            RwLock::new(file)
        })
        .write()
        .expect("python dataclasses rwlock poisioned");
    writeln!(writer, "{}", item).expect("failed to write pygen");
}

struct Dataclass {
    indent: usize,
    name: String,
    inclasses: Vec<Dataclass>,
    items: Vec<DataclassItem>,
    init: Option<DataclassInit>,
}

impl Dataclass {
    pub fn new(name: String) -> Self {
        Self {
            indent: 0,
            name,
            inclasses: vec![],
            items: vec![],
            init: None,
        }
    }

    pub fn add_inclass(&mut self, mut inclass: Dataclass) {
        inclass.set_indent(self.indent + 4);
        self.inclasses.push(inclass)
    }

    pub fn add_item(&mut self, mut item: DataclassItem) {
        item.set_indent(self.indent + 4);
        self.items.push(item)
    }

    pub fn set_init(&mut self, init: Option<DataclassInit>) {
        self.init = init.map(|mut x| {
            x.set_indent(self.indent + 4);
            x
        });
    }

    pub fn set_indent(&mut self, mut indent: usize) -> usize {
        for inclass in &mut self.inclasses {
            inclass.set_indent(indent + 4);
        }
        for item in &mut self.items {
            item.set_indent(indent + 4);
        }
        if let Some(init) = &mut self.init {
            init.set_indent(indent + 4);
        }

        std::mem::swap(&mut self.indent, &mut indent);
        indent
    }
}

impl Display for Dataclass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = self.name.as_str();
        let indent_str = " ".repeat(self.indent);
        let gen_init = match self.init {
            Some(_) => "False",
            None => "True",
        };

        // write class signature
        writeln!(
            f,
            "{indent_str}@dataclass(init={gen_init})\n\
            {indent_str}class {name}:"
        )?;

        // write inner class definitions
        for inclass in &self.inclasses {
            writeln!(f, "{inclass}",)?;
        }

        // write dataclass fields
        for item in &self.items {
            writeln!(f, "{item}")?;
        }

        // write init definition (if any)
        if let Some(init) = &self.init {
            write!(f, "{init}")?;
        }

        Ok(())
    }
}

struct DataclassItem {
    indent: usize,
    name: String,
    ty: String,
}

impl DataclassItem {
    pub fn new(name: String, ty: String) -> Self {
        Self {
            indent: 0,
            name,
            ty,
        }
    }

    pub fn set_indent(&mut self, mut indent: usize) -> usize {
        std::mem::swap(&mut self.indent, &mut indent);
        indent
    }
}

impl Display for DataclassItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let &Self { name, ty, .. } = &self;
        let indent_str = " ".repeat(self.indent);
        write!(f, "{indent_str}{name}: {ty}")
    }
}

struct DataclassInit {
    indent: usize,
    args: InitArgs,
    body: Vec<String>,
}

impl DataclassInit {
    pub fn new() -> Self {
        Self {
            indent: 0,
            args: InitArgs::new(),
            body: vec![],
        }
    }

    pub fn add_line(&mut self, line: String) {
        self.body.push(line)
    }

    pub fn set_args(&mut self, args: InitArgs) {
        self.args = args;
    }

    pub fn set_indent(&mut self, mut indent: usize) -> usize {
        std::mem::swap(&mut self.indent, &mut indent);
        indent
    }
}

impl Display for DataclassInit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let &Self { args, .. } = &self;
        let indent_str = " ".repeat(self.indent);

        writeln!(f, "{indent_str}def __init__(self{args}):")?;

        for line in &self.body {
            writeln!(f, "{indent_str}    {line}")?;
        }

        Ok(())
    }
}

struct InitArgs {
    pairs: Vec<(String, String)>,
}

impl InitArgs {
    pub fn new() -> Self {
        Self { pairs: vec![] }
    }

    pub fn add(&mut self, name: String, ty: String) {
        self.pairs.push((name, ty))
    }
}

impl Display for InitArgs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for arg in &self.pairs {
            let (name, ty) = arg;
            write!(f, ", {name}: {ty}")?;
        }
        Ok(())
    }
}
