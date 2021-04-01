// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A Fuchsia Driver Bind Program compiler

use anyhow::{anyhow, Context, Error};
use bind::compiler::{self, BindProgram, SymbolicInstruction, SymbolicInstructionInfo};
use bind::encode_bind_program_v1::encode_to_string_v1;
use bind::encode_bind_program_v2::encode_to_string_v2;
use bind::{bind_library, linter, offline_debugger, test};
use std::collections::{HashMap, HashSet};
use std::convert::TryFrom;
use std::fmt::Write;
use std::fs::File;
use std::io::prelude::*;
use std::io::{self, BufRead, Write as IoWrite};
use std::path::PathBuf;
use structopt::StructOpt;

#[derive(StructOpt, Debug)]
struct SharedOptions {
    /// The bind library input files. These may be included by the bind program. They should be in
    /// the format described in //tools/bindc/README.md.
    #[structopt(short = "i", long = "include", parse(from_os_str))]
    include: Vec<PathBuf>,

    /// Specifiy the bind library input files as a file. The file must contain a list of filenames
    /// that are bind library input files that may be included by the bind program. Those files
    /// should be in the format described in //tools/bindc/README.md.
    #[structopt(short = "f", long = "include-file", parse(from_os_str))]
    include_file: Option<PathBuf>,

    /// The bind program input file. This should be in the format described in
    /// //tools/bindc/README.md. This is required unless disable_autobind is true, in which case
    /// the driver while bind unconditionally (but only on the user's request.)
    #[structopt(parse(from_os_str))]
    input: Option<PathBuf>,

    /// Check inputs for style guide violations.
    #[structopt(short = "l", long = "lint")]
    lint: bool,
}

#[derive(StructOpt, Debug)]
enum Command {
    #[structopt(name = "compile")]
    Compile {
        #[structopt(flatten)]
        options: SharedOptions,

        /// Output file. The compiler emits a C header file.
        #[structopt(short = "o", long = "output", parse(from_os_str))]
        output: Option<PathBuf>,

        /// Specify a path for the compiler to generate a depfile. A depfile contain, in Makefile
        /// format, the files that this invocation of the compiler depends on including all bind
        /// libraries and the bind program input itself. An output file must be provided to generate
        /// a depfile.
        #[structopt(short = "d", long = "depfile", parse(from_os_str))]
        depfile: Option<PathBuf>,

        // TODO(fxbug.dev/43400): Eventually this option should be removed when we can define this
        // configuration in the driver's component manifest.
        /// Disable automatically binding the driver so that the driver must be bound on a user's
        /// request.
        #[structopt(short = "a", long = "disable-autobind")]
        disable_autobind: bool,

        /// Output a bytecode file, instead of a C header file.
        #[structopt(short = "b", long = "output-bytecode")]
        output_bytecode: bool,

        /// Encode the bytecode in the new format if true. Otherwise, encode to the old format.
        /// Currently the new bytecode format is unimplemented. See fxb/67440.
        #[structopt(short = "n", long = "use-new-bytecode")]
        use_new_bytecode: bool,
    },
    #[structopt(name = "debug")]
    Debug {
        #[structopt(flatten)]
        options: SharedOptions,

        /// A file containing the properties of a specific device, as a list of key-value pairs.
        /// This will be used as the input to the bind program debugger.
        #[structopt(short = "d", long = "debug", parse(from_os_str))]
        device_file: PathBuf,
    },
    #[structopt(name = "test")]
    Test {
        #[structopt(flatten)]
        options: SharedOptions,

        // TODO(fxbug.dev/56774): Refer to documentation for bind testing.
        /// A file containing the test specification.
        #[structopt(short = "t", long = "test-spec", parse(from_os_str))]
        test_spec: PathBuf,
    },
    #[structopt(name = "generate")]
    Generate {
        #[structopt(flatten)]
        options: SharedOptions,

        /// Output FIDL file.
        #[structopt(short = "o", long = "output", parse(from_os_str))]
        output: Option<PathBuf>,
    },
}

fn main() {
    let command = Command::from_iter(std::env::args());
    if let Err(err) = handle_command(command) {
        eprintln!("{}", err);
        std::process::exit(1);
    }
}

fn write_depfile(
    output: &PathBuf,
    input: &Option<PathBuf>,
    includes: &[PathBuf],
) -> Result<String, Error> {
    fn path_to_str(path: &PathBuf) -> Result<&str, Error> {
        path.as_os_str().to_str().context("failed to convert path to string")
    }

    let mut deps = includes.iter().map(|s| path_to_str(s)).collect::<Result<Vec<&str>, Error>>()?;

    if let Some(input) = input {
        let input_str = path_to_str(input)?;
        deps.push(input_str);
    }

    let output_str = path_to_str(output)?;
    let mut out = String::new();
    writeln!(&mut out, "{}: {}", output_str, deps.join(" "))?;
    Ok(out)
}

fn write_bind_template<'a>(bind_program: BindProgram<'a>) -> Result<String, Error> {
    let mut output = String::new();
    if bind_program.use_new_bytecode {
        let (binding, byte_count) = encode_to_string_v2(bind_program)?;
        output
            .write_fmt(format_args!(
                include_str!("templates/bind_v2.h.template"),
                byte_count = byte_count,
                binding = binding,
            ))
            .context("Failed to format output")?;
    } else {
        output
            .write_fmt(format_args!(
                include_str!("templates/bind_v1.h.template"),
                bind_count = bind_program.instructions.len(),
                binding = encode_to_string_v1(bind_program)?,
            ))
            .context("Failed to format output")?;
    }
    Ok(output)
}

fn read_file(path: &PathBuf) -> Result<String, Error> {
    let mut file = File::open(path)?;
    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    Ok(buf)
}

fn handle_command(command: Command) -> Result<(), Error> {
    match command {
        Command::Debug { options, device_file } => {
            let includes = handle_includes(options.include, options.include_file)?;
            let includes = includes.iter().map(read_file).collect::<Result<Vec<String>, _>>()?;
            let input = options.input.ok_or(anyhow!("The debug command requires an input."))?;
            let program = read_file(&input)?;
            let bind_program = compiler::compile(&program, &includes, options.lint, false)?;

            let device = read_file(&device_file)?;
            let binds = offline_debugger::debug_from_str(&bind_program, &device)?;
            if binds {
                println!("Driver binds to device.");
            } else {
                println!("Driver doesn't bind to device.");
            }
            Ok(())
        }
        Command::Test { options, test_spec } => {
            let input = options.input.ok_or(anyhow!("The test command requires an input."))?;
            let program = read_file(&input)?;
            let includes = handle_includes(options.include, options.include_file)?;
            let includes = includes.iter().map(read_file).collect::<Result<Vec<String>, _>>()?;
            let test_spec = read_file(&test_spec)?;
            if !test::run(&program, &includes, &test_spec)? {
                return Err(anyhow!("Test failed"));
            }
            Ok(())
        }
        Command::Compile {
            options,
            output,
            depfile,
            disable_autobind,
            output_bytecode,
            use_new_bytecode,
        } => {
            let includes = handle_includes(options.include, options.include_file)?;
            handle_compile(
                options.input,
                includes,
                disable_autobind,
                output_bytecode,
                use_new_bytecode,
                options.lint,
                output,
                depfile,
            )
        }
        Command::Generate { options, output } => {
            handle_generate(options.input, options.lint, output)
        }
    }
}

fn handle_includes(
    mut includes: Vec<PathBuf>,
    include_file: Option<PathBuf>,
) -> Result<Vec<PathBuf>, Error> {
    if let Some(include_file) = include_file {
        let file = File::open(include_file).context("Failed to open include file")?;
        let reader = io::BufReader::new(file);
        let mut filenames = reader
            .lines()
            .map(|line| line.map(PathBuf::from))
            .map(|line| line.context("Failed to read include file"))
            .collect::<Result<Vec<_>, Error>>()?;
        includes.append(&mut filenames);
    }
    Ok(includes)
}

fn handle_compile(
    input: Option<PathBuf>,
    includes: Vec<PathBuf>,
    disable_autobind: bool,
    output_bytecode: bool,
    use_new_bytecode: bool,
    lint: bool,
    output: Option<PathBuf>,
    depfile: Option<PathBuf>,
) -> Result<(), Error> {
    let mut output_writer: Box<dyn io::Write> = if let Some(output) = output {
        // If there's an output filename then we can generate a depfile too.
        if let Some(filename) = depfile {
            let mut file = File::create(filename).context("Failed to open depfile")?;
            let depfile_string =
                write_depfile(&output, &input, &includes).context("Failed to create depfile")?;
            file.write(depfile_string.as_bytes()).context("Failed to write to depfile")?;
        }
        Box::new(File::create(output).context("Failed to create output file")?)
    } else {
        Box::new(io::stdout())
    };

    let program;
    let bind_program = if !disable_autobind {
        let input = input.ok_or(anyhow!("An input is required when disable_autobind is false."))?;
        program = read_file(&input)?;
        let includes = includes.iter().map(read_file).collect::<Result<Vec<String>, _>>()?;
        compiler::compile(&program, &includes, lint, use_new_bytecode)?
    } else if let Some(input) = input {
        // Autobind is disabled but there are some bind rules for manual binding.
        program = read_file(&input)?;
        let includes = includes.iter().map(read_file).collect::<Result<Vec<String>, _>>()?;
        let mut bind_program = compiler::compile(&program, &includes, lint, use_new_bytecode)?;
        bind_program.instructions.insert(0, SymbolicInstructionInfo::disable_autobind());
        bind_program
    } else {
        // Autobind is disabled and there are no bind rules. Emit only the autobind check.
        // Since the new bytecode format doesn't support match instructions, only add the
        // UnconditionalBind instruction to the old bytecode.
        let instructions = if use_new_bytecode {
            vec![SymbolicInstructionInfo::disable_autobind()]
        } else {
            vec![
                SymbolicInstructionInfo::disable_autobind(),
                SymbolicInstructionInfo {
                    location: None,
                    instruction: SymbolicInstruction::UnconditionalBind,
                },
            ]
        };

        BindProgram {
            instructions: instructions,
            symbol_table: HashMap::new(),
            use_new_bytecode: use_new_bytecode,
        }
    };

    if output_bytecode {
        let bytecode = bind_program.encode_to_bytecode()?;
        output_writer.write_all(bytecode.as_slice()).context("Failed to write to output file")?;
    } else {
        let template = write_bind_template(bind_program)?;
        output_writer.write_all(template.as_bytes()).context("Failed to write to output file")?;
    };

    Ok(())
}

fn generate_declaration_name(name: &String, value: &bind_library::Value) -> String {
    match value {
        bind_library::Value::Number(value_name, _) => {
            format!("{}_{}", name, value_name)
        }
        bind_library::Value::Str(value_name, _) => {
            format!("{}_{}", name, value_name)
        }
        bind_library::Value::Bool(value_name, _) => {
            format!("{}_{}", name, value_name)
        }
        bind_library::Value::Enum(value_name) => {
            format!("{}_{}", name, value_name)
        }
    }
    .to_uppercase()
}

/// The generated identifiers for each value must be unique. Since the key and value identifiers
/// are joined using underscores which are also valid to use in the identifiers themselves,
/// duplicate keys may be produced. I.e. the key-value pair "A_B" and "C", and the key-value pair
/// "A" and "B_C", will both produce the identifier "A_B_C". This function hence ensures none of the
/// generated names are duplicates.
fn check_names(declarations: &Vec<bind_library::Declaration>) -> Result<(), Error> {
    let mut names: HashSet<String> = HashSet::new();

    for declaration in declarations.into_iter() {
        for value in &declaration.values {
            let name = generate_declaration_name(&declaration.identifier.name, value);

            // Return an error if there is a duplicate name.
            if names.contains(&name) {
                return Err(anyhow!("Name \"{}\" generated for more than one key", name));
            }

            names.insert(name);
        }
    }

    Ok(())
}

/// Converts a declaration to the FIDL constant format.
fn convert_to_fidl_constant(
    declaration: bind_library::Declaration,
    path: &String,
) -> Result<String, Error> {
    let mut result = String::new();
    let identifier_name = declaration.identifier.name.to_uppercase();

    // Generating the key definition is only done when it is not extended.
    // When it is extended, the key will already be defined in the library that it is
    // extending from.
    if !declaration.extends {
        writeln!(
            &mut result,
            "const fdf.NodePropertyKey {} = \"{}.{}\";",
            &identifier_name, &path, &identifier_name
        )?;
    }

    for value in &declaration.values {
        let name = generate_declaration_name(&identifier_name, value);
        let property_output = match &value {
            bind_library::Value::Number(_, val) => {
                format!("const fdf.NodePropertyValueUint {} = {};", name, val)
            }
            bind_library::Value::Str(_, val) => {
                format!("const fdf.NodePropertyValueString {} = \"{}\";", name, val)
            }
            bind_library::Value::Bool(_, val) => {
                format!("const fdf.NodePropertyValueBool {} = {};", name, val)
            }
            bind_library::Value::Enum(_) => {
                format!("const fdf.NodePropertyValueEnum {};", name)
            }
        };
        writeln!(&mut result, "{}", property_output)?;
    }

    Ok(result)
}

fn write_fidl_template(syntax_tree: bind_library::Ast) -> Result<String, Error> {
    // Use the bind library name as the FIDL library name and give it "bind" as a top level
    // namespace.
    let bind_name = &syntax_tree.name.to_string();
    let library_name = format!("bind.{}", bind_name);

    check_names(&syntax_tree.declarations)?;

    // Convert all key value pairs to their equivalent constants.
    let definition = syntax_tree
        .declarations
        .into_iter()
        .map(|declaration| convert_to_fidl_constant(declaration, bind_name))
        .collect::<Result<Vec<String>, _>>()?
        .join("\n");

    // Output result into template.
    let mut output = String::new();
    output
        .write_fmt(format_args!(
            include_str!("templates/fidl.template"),
            library_name = library_name,
            definition = definition,
        ))
        .context("Failed to format output")?;

    Ok(output.to_string())
}

fn handle_generate(
    input: Option<PathBuf>,
    lint: bool,
    output: Option<PathBuf>,
) -> Result<(), Error> {
    let input = input.ok_or(anyhow!("An input is required."))?;
    let input_content = read_file(&input)?;

    // Generate the FIDL library.
    let keys = bind_library::Ast::try_from(input_content.as_str())
        .map_err(compiler::CompilerError::BindParserError)?;
    if lint {
        linter::lint_library(&keys).map_err(compiler::CompilerError::LinterError)?;
    }
    let template = write_fidl_template(keys)?;

    // Create and open output file.
    let mut output_writer: Box<dyn io::Write> = if let Some(output) = output {
        Box::new(File::create(output).context("Failed to create output file.")?)
    } else {
        // Output file name was not given. Print result to stdout.
        Box::new(io::stdout())
    };

    // Write FIDL library to output.
    output_writer.write_all(template.as_bytes()).context("Failed to write to output file")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get_test_fidl_template(ast: bind_library::Ast) -> Vec<String> {
        write_fidl_template(ast)
            .unwrap()
            .split("\n")
            .map(|s| s.to_string())
            .filter(|x| !x.is_empty())
            .collect()
    }

    #[test]
    fn zero_instructions_v1() {
        let bind_program = BindProgram {
            instructions: vec![],
            symbol_table: HashMap::new(),
            use_new_bytecode: false,
        };

        let bytecode = bind_program.encode_to_bytecode().unwrap();
        assert!(bytecode.is_empty());

        let bind_program = BindProgram {
            instructions: vec![],
            symbol_table: HashMap::new(),
            use_new_bytecode: false,
        };
        let template = write_bind_template(bind_program).unwrap();
        assert!(
            template.contains("ZIRCON_DRIVER_BEGIN_PRIV_V1(Driver, Ops, VendorName, Version, 0)")
        );
    }

    #[test]
    fn one_instruction_v1() {
        let bind_program = BindProgram {
            instructions: vec![SymbolicInstructionInfo {
                location: None,
                instruction: SymbolicInstruction::UnconditionalBind,
            }],
            symbol_table: HashMap::new(),
            use_new_bytecode: false,
        };

        let bytecode = bind_program.encode_to_bytecode().unwrap();
        assert_eq!(bytecode, vec![0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0]);

        let bind_program = BindProgram {
            instructions: vec![SymbolicInstructionInfo {
                location: None,
                instruction: SymbolicInstruction::UnconditionalBind,
            }],
            symbol_table: HashMap::new(),
            use_new_bytecode: false,
        };
        let template = write_bind_template(bind_program).unwrap();
        assert!(
            template.contains("ZIRCON_DRIVER_BEGIN_PRIV_V1(Driver, Ops, VendorName, Version, 1)")
        );
        assert!(template.contains("{0x1000000,0x0,0x0}"));
    }

    #[test]
    fn zero_instructions_v2() {
        let bind_program = BindProgram {
            instructions: vec![],
            symbol_table: HashMap::new(),
            use_new_bytecode: true,
        };
        assert_eq!(
            bind_program.encode_to_bytecode().unwrap(),
            vec![
                66, 73, 78, 68, 2, 0, 0, 0, 83, 89, 78, 66, 0, 0, 0, 0, 73, 78, 83, 84, 0, 0, 0, 0
            ]
        );

        let bind_program = BindProgram {
            instructions: vec![],
            symbol_table: HashMap::new(),
            use_new_bytecode: true,
        };
        let template = write_bind_template(bind_program).unwrap();
        assert!(
            template.contains("ZIRCON_DRIVER_BEGIN_PRIV_V2(Driver, Ops, VendorName, Version, 24)")
        );
        assert!(template.contains(
            "0x42,0x49,0x4e,0x44,0x2,0x0,0x0,0x0,0x53,0x59,0x4e,0x42,0x0,\
             0x0,0x0,0x0,0x49,0x4e,0x53,0x54,0x0,0x0,0x0,0x0"
        ));
    }

    #[test]
    fn one_instruction_v2() {
        let bind_program = BindProgram {
            instructions: vec![SymbolicInstructionInfo {
                location: None,
                instruction: SymbolicInstruction::UnconditionalAbort,
            }],
            symbol_table: HashMap::new(),
            use_new_bytecode: true,
        };
        assert_eq!(
            bind_program.encode_to_bytecode().unwrap(),
            vec![
                66, 73, 78, 68, 2, 0, 0, 0, 83, 89, 78, 66, 0, 0, 0, 0, 73, 78, 83, 84, 1, 0, 0, 0,
                48
            ]
        );

        let bind_program = BindProgram {
            instructions: vec![SymbolicInstructionInfo {
                location: None,
                instruction: SymbolicInstruction::UnconditionalAbort,
            }],
            symbol_table: HashMap::new(),
            use_new_bytecode: true,
        };
        let template = write_bind_template(bind_program).unwrap();
        assert!(
            template.contains("ZIRCON_DRIVER_BEGIN_PRIV_V2(Driver, Ops, VendorName, Version, 25)")
        );
        assert!(template.contains(
            "0x42,0x49,0x4e,0x44,0x2,0x0,0x0,0x0,0x53,0x59,0x4e,0x42,0x0,0x0,\
             0x0,0x0,0x49,0x4e,0x53,0x54,0x1,0x0,0x0,0x0,0x30"
        ));
    }

    #[test]
    fn disable_autobind() {
        let bind_program = BindProgram {
            instructions: vec![
                SymbolicInstructionInfo::disable_autobind(),
                SymbolicInstructionInfo {
                    location: None,
                    instruction: SymbolicInstruction::UnconditionalBind,
                },
            ],
            symbol_table: HashMap::new(),
            use_new_bytecode: false,
        };

        let bytecode = bind_program.encode_to_bytecode().unwrap();
        assert_eq!(bytecode[..12], [2, 0, 0, 0x20, 0, 0, 0, 0, 0, 0, 0, 0]);

        let bind_program = BindProgram {
            instructions: vec![
                SymbolicInstructionInfo::disable_autobind(),
                SymbolicInstructionInfo {
                    location: None,
                    instruction: SymbolicInstruction::UnconditionalBind,
                },
            ],
            symbol_table: HashMap::new(),
            use_new_bytecode: false,
        };
        let template = write_bind_template(bind_program).unwrap();
        assert!(
            template.contains("ZIRCON_DRIVER_BEGIN_PRIV_V1(Driver, Ops, VendorName, Version, 2)")
        );
        assert!(template.contains("{0x20000002,0x0,0x0}"));
    }

    #[test]
    fn depfile_no_includes() {
        let output = PathBuf::from("/a/output");
        let input = PathBuf::from("/a/input");
        assert_eq!(
            write_depfile(&output, &Some(input), &[]).unwrap(),
            "/a/output: /a/input\n".to_string()
        );
    }

    #[test]
    fn depfile_no_input() {
        let output = PathBuf::from("/a/output");
        let includes = vec![PathBuf::from("/a/include"), PathBuf::from("/b/include")];
        let result = write_depfile(&output, &None, &includes).unwrap();
        assert!(result.starts_with("/a/output:"));
        assert!(result.contains("/a/include"));
        assert!(result.contains("/b/include"));
    }

    #[test]
    fn depfile_input_and_includes() {
        let output = PathBuf::from("/a/output");
        let input = PathBuf::from("/a/input");
        let includes = vec![PathBuf::from("/a/include"), PathBuf::from("/b/include")];
        let result = write_depfile(&output, &Some(input), &includes).unwrap();
        assert!(result.starts_with("/a/output:"));
        assert!(result.contains("/a/input"));
        assert!(result.contains("/a/include"));
        assert!(result.contains("/b/include"));
    }

    #[test]
    fn zero_keys() {
        let empty_ast = bind_library::Ast::try_from("library fuchsia.platform;").unwrap();
        let template: Vec<String> = get_test_fidl_template(empty_ast);

        let expected = vec![
            "[NoDoc]".to_string(),
            "library bind.fuchsia.platform;".to_string(),
            "using fuchsia.driver.framework as fdf;".to_string(),
        ];

        assert!(template.into_iter().zip(expected).all(|(a, b)| (a == b)));
    }

    #[test]
    fn one_key() {
        let ast = bind_library::Ast::try_from(
            "library fuchsia.platform;\nstring A_KEY {\nA_VALUE = \"a string value\",\n};",
        )
        .unwrap();
        let template: Vec<String> = get_test_fidl_template(ast);

        let expected = vec![
            "[NoDoc]".to_string(),
            "library bind.fuchsia.platform;".to_string(),
            "using fuchsia.driver.framework as fdf;".to_string(),
            "const fdf.NodePropertyKey A_KEY = \"fuchsia.platform.A_KEY\";".to_string(),
            "const fdf.NodePropertyValueString A_KEY_A_VALUE = \"a string value\";".to_string(),
        ];

        assert!(template.into_iter().zip(expected).all(|(a, b)| (a == b)));
    }

    #[test]
    fn one_key_extends() {
        let ast = bind_library::Ast::try_from(
            "library fuchsia.platform;\nextend uint fuchsia.BIND_PROTOCOL {\nBUS = 84,\n};",
        )
        .unwrap();
        let template: Vec<String> = get_test_fidl_template(ast);

        let expected = vec![
            "[NoDoc]".to_string(),
            "library bind.fuchsia.platform;".to_string(),
            "using fuchsia.driver.framework as fdf;".to_string(),
            "const fdf.NodePropertyValueUint BIND_PROTOCOL_BUS = 84;".to_string(),
        ];

        assert!(template.into_iter().zip(expected).all(|(a, b)| (a == b)));
    }

    #[test]
    fn lower_snake_case() {
        let ast = bind_library::Ast::try_from(
            "library fuchsia.platform;\nstring a_key {\na_value = \"a string value\",\n};",
        )
        .unwrap();
        let template: Vec<String> = get_test_fidl_template(ast);

        let expected = vec![
            "[NoDoc]".to_string(),
            "library bind.fuchsia.platform;".to_string(),
            "using fuchsia.driver.framework as fdf;".to_string(),
            "const fdf.NodePropertyKey A_KEY = \"fuchsia.platform.A_KEY\";".to_string(),
            "const fdf.NodePropertyValueString A_KEY_A_VALUE = \"a string value\";".to_string(),
        ];

        assert!(template.into_iter().zip(expected).all(|(a, b)| (a == b)));
    }

    #[test]
    fn duplicate_key_value() {
        let ast = bind_library::Ast::try_from(
            "library fuchsia.platform;\nstring A_KEY {\nA_VALUE = \"a string value\",\n};
            \nstring A_KEY_A {\nVALUE = \"a string value\",\n};",
        )
        .unwrap();
        let template = write_fidl_template(ast);

        assert!(template.is_err());
    }

    #[test]
    fn duplicate_values_one_key() {
        let ast = bind_library::Ast::try_from(
            "library fuchsia.platform;\nstring A_KEY {\nA_VALUE = \"a string value\",\n
            A_VALUE = \"a string value\",\n};",
        )
        .unwrap();
        let template = write_fidl_template(ast);

        assert!(template.is_err());
    }

    #[test]
    fn duplicate_values_two_keys() {
        let ast = bind_library::Ast::try_from(
            "library fuchsia.platform;\nstring KEY {\nA_VALUE = \"a string value\",\n};\n
            string KEY_A {\nVALUE = \"a string value\",\n};\n",
        )
        .unwrap();
        let template = write_fidl_template(ast);

        assert!(template.is_err());
    }
}
