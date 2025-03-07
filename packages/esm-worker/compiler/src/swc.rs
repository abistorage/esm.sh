use crate::error::{DiagnosticBuffer, ErrorBuffer};
use crate::export_names::ExportParser;
use crate::resolve_fold::resolve_fold;
use crate::resolver::{DependencyDescriptor, Resolver};
use crate::source_type::SourceType;

use std::{cell::RefCell, path::Path, rc::Rc};
use swc_common::{
	chain,
	comments::SingleThreadedComments,
	errors::{Handler, HandlerFlags},
	FileName, Globals, Mark, SourceMap,
};
use swc_ecma_transforms_proposal::decorators;
use swc_ecma_transforms_typescript::strip;
use swc_ecmascript::{
	ast::{Module, Program},
	codegen::{text_writer::JsWriter, Node},
	parser::{lexer::Lexer, EsConfig, JscTarget, StringInput, Syntax, TsConfig},
	transforms::{fixer, helpers, hygiene, pass::Optional, react, resolver_with_mark},
	visit::{Fold, FoldWith},
};

/// Options for transpiling a module.
#[derive(Debug, Clone)]
pub struct EmitOptions {
	pub jsx_factory: String,
	pub jsx_fragment_factory: String,
	pub source_map: bool,
	pub is_dev: bool,
}

impl Default for EmitOptions {
	fn default() -> Self {
		EmitOptions {
			jsx_factory: "React.createElement".into(),
			jsx_fragment_factory: "React.Fragment".into(),
			is_dev: false,
			source_map: false,
		}
	}
}

#[derive(Clone)]
pub struct SWC {
	pub specifier: String,
	pub module: Module,
	pub source_type: SourceType,
	pub source_map: Rc<SourceMap>,
	pub comments: SingleThreadedComments,
}

impl SWC {
	/// parse source code.
	pub fn parse(
		specifier: &str,
		source: &str,
		source_type: Option<SourceType>,
	) -> Result<Self, anyhow::Error> {
		let source_map = SourceMap::default();
		let source_file = source_map.new_source_file(
			FileName::Real(Path::new(specifier).to_path_buf()),
			source.into(),
		);
		let sm = &source_map;
		let error_buffer = ErrorBuffer::new(specifier);
		let source_type = match source_type {
			Some(source_type) => match source_type {
				SourceType::Unknown => SourceType::from(Path::new(specifier)),
				_ => source_type,
			},
			None => SourceType::from(Path::new(specifier)),
		};
		let syntax = get_syntax(&source_type);
		let input = StringInput::from(&*source_file);
		let comments = SingleThreadedComments::default();
		let lexer = Lexer::new(syntax, JscTarget::Es2020, input, Some(&comments));
		let mut parser = swc_ecmascript::parser::Parser::new_from(lexer);
		let handler = Handler::with_emitter_and_flags(
			Box::new(error_buffer.clone()),
			HandlerFlags {
				can_emit_warnings: true,
				dont_buffer_diagnostics: true,
				..HandlerFlags::default()
			},
		);
		let module = parser
			.parse_module()
			.map_err(move |err| {
				let mut diagnostic = err.into_diagnostic(&handler);
				diagnostic.emit();
				DiagnosticBuffer::from_error_buffer(error_buffer, |span| sm.lookup_char_pos(span.lo))
			})
			.unwrap();

		Ok(SWC {
			specifier: specifier.into(),
			module,
			source_type,
			source_map: Rc::new(source_map),
			comments,
		})
	}

	/// parse export names in the module.
	pub fn parse_export_names(&self) -> Result<Vec<String>, anyhow::Error> {
		let program = Program::Module(self.module.clone());
		let mut parser = ExportParser { names: vec![] };
		program.fold_with(&mut parser);
		Ok(parser.names)
	}

	/// transform a JS/TS/JSX/TSX file into a JS file, based on the supplied options.
	pub fn transform(
		self,
		resolver: Rc<RefCell<Resolver>>,
		options: &EmitOptions,
	) -> Result<(String, Option<String>), anyhow::Error> {
		swc_common::GLOBALS.set(&Globals::new(), || {
			let top_level_mark = Mark::fresh(Mark::root());
			let specifier_is_remote = resolver.borrow().specifier_is_remote;
			let jsx = match self.source_type {
				SourceType::JSX => true,
				SourceType::TSX => true,
				_ => false,
			};
			let passes = chain!(
				Optional::new(
					react::refresh(
						true,
						Some(react::RefreshOptions {
							refresh_reg: "$RefreshReg$".into(),
							refresh_sig: "$RefreshSig$".into(),
							emit_full_signatures: false,
						}),
						self.source_map.clone(),
						Some(&self.comments),
					),
					options.is_dev && !specifier_is_remote
				),
				Optional::new(resolver_with_mark(top_level_mark), jsx),
				Optional::new(
					react::jsx(
						self.source_map.clone(),
						Some(&self.comments),
						react::Options {
							pragma: options.jsx_factory.clone(),
							pragma_frag: options.jsx_fragment_factory.clone(),
							// this will use `Object.assign()` instead of the `_extends` helper when spreading props.
							use_builtins: true,
							..Default::default()
						},
						top_level_mark
					),
					jsx
				),
				resolve_fold(resolver.clone(), options.is_dev),
				decorators::decorators(decorators::Config {
					legacy: true,
					emit_metadata: false
				}),
				helpers::inject_helpers(),
				strip::strip_with_config(strip::Config {
					use_define_for_class_fields: true,
					..Default::default()
				}),
				fixer(Some(&self.comments)),
				hygiene()
			);

			let (code, map) = self.apply_fold(passes, options.source_map).unwrap();
			let mut resolver = resolver.borrow_mut();

			// remove unused deps by tree-shaking
			let mut deps: Vec<DependencyDescriptor> = Vec::new();
			for dep in resolver.deps.clone() {
				if resolver.star_exports.contains(&dep.specifier)
					|| code.contains(to_str_lit(dep.specifier.as_str()).as_str())
				{
					deps.push(dep);
				}
			}
			resolver.deps = deps;

			Ok((code, map))
		})
	}

	/// Apply transform with the fold.
	pub fn apply_fold<T: Fold>(
		&self,
		mut fold: T,
		source_map: bool,
	) -> Result<(String, Option<String>), anyhow::Error> {
		let program = Program::Module(self.module.clone());
		let program = helpers::HELPERS.set(&helpers::Helpers::new(false), || {
			program.fold_with(&mut fold)
		});
		let mut buf = Vec::new();
		let mut src_map_buf = Vec::new();
		let src_map = if source_map {
			Some(&mut src_map_buf)
		} else {
			None
		};
		{
			let writer = Box::new(JsWriter::new(
				self.source_map.clone(),
				"\n",
				&mut buf,
				src_map,
			));
			let mut emitter = swc_ecmascript::codegen::Emitter {
				cfg: swc_ecmascript::codegen::Config {
					minify: false, // todo: use swc minify in the future, currently use terser
				},
				comments: Some(&self.comments),
				cm: self.source_map.clone(),
				wr: writer,
			};
			program.emit_with(&mut emitter).unwrap();
		}

		// output
		let src = String::from_utf8(buf).unwrap();
		if source_map {
			let mut buf = Vec::new();
			self
				.source_map
				.build_source_map_from(&mut src_map_buf, None)
				.to_writer(&mut buf)
				.unwrap();
			Ok((src, Some(String::from_utf8(buf).unwrap())))
		} else {
			Ok((src, None))
		}
	}
}

fn get_es_config(jsx: bool) -> EsConfig {
	EsConfig {
		class_private_methods: true,
		class_private_props: true,
		class_props: true,
		dynamic_import: true,
		export_default_from: true,
		export_namespace_from: true,
		num_sep: true,
		nullish_coalescing: true,
		optional_chaining: true,
		top_level_await: true,
		import_meta: true,
		import_assertions: true,
		jsx,
		..EsConfig::default()
	}
}

fn get_ts_config(tsx: bool) -> TsConfig {
	TsConfig {
		decorators: true,
		dynamic_import: true,
		tsx,
		..TsConfig::default()
	}
}

fn get_syntax(source_type: &SourceType) -> Syntax {
	match source_type {
		SourceType::JS => Syntax::Es(get_es_config(false)),
		SourceType::JSX => Syntax::Es(get_es_config(true)),
		SourceType::TS => Syntax::Typescript(get_ts_config(false)),
		SourceType::TSX => Syntax::Typescript(get_ts_config(true)),
		_ => Syntax::Es(get_es_config(false)),
	}
}

fn to_str_lit(sub_text: &str) -> String {
	let mut s = "\"".to_owned();
	s.push_str(sub_text);
	s.push('"');
	s
}
#[cfg(test)]
mod tests {
	use super::*;
	use crate::import_map::ImportHashMap;

	fn st(specifer: &str, source: &str, bundle_mode: bool) -> (String, Rc<RefCell<Resolver>>) {
		let module = SWC::parse(specifer, source, None).expect("could not parse module");
		let resolver = Rc::new(RefCell::new(Resolver::new(
			specifer,
			ImportHashMap::default(),
			bundle_mode,
			vec![],
			None,
		)));
		let (code, _) = module
			.transform(resolver.clone(), &EmitOptions::default())
			.unwrap();
		println!("{}", code);
		(code, resolver)
	}

	#[test]
	fn typescript() {
		let source = r#"
      enum D {
        A,
        B,
        C,
      }

      function enumerable(value: boolean) {
        return function (
          _target: any,
          _propertyKey: string,
          descriptor: PropertyDescriptor,
        ) {
          descriptor.enumerable = value;
        };
      }

      export class A {
        private b: string;
        protected c: number = 1;
        e: "foo";
        constructor (public d = D.A) {
          const e = "foo" as const;
          this.e = e;
        }
        @enumerable(false)
        bar() {}
      }
    "#;
		let (code, _) = st("https://deno.land/x/mod.ts", source, false);
		assert!(code.contains("var D;\n(function(D) {\n"));
		assert!(code.contains("_applyDecoratedDescriptor("));
	}

	#[test]
	fn react_jsx() {
		let source = r#"
      import React from "https://esm.sh/react"
      export default function App() {
        return (
          <>
            <h1 className="title">Hello World</h1>
          </>
        )
      }
    "#;
		let (code, _) = st("app.tsx", source, false);
		assert!(code.contains("React.createElement(React.Fragment, null"));
		assert!(code.contains("React.createElement(\"h1\", {"));
		assert!(code.contains("className: \"title\""));
	}

	#[test]
	fn parse_export_names() {
		let source = r#"
      export const name = "alephjs"
      export const version = "1.0.1"
      const start = () => {}
      export default start
      export const { build } = { build: () => {} }
      export function dev() {}
      export class Server {}
      export const { a: { a1, a2 }, 'b': [ b1, b2 ], c, ...rest } = { a: { a1: 0, a2: 0 }, b: [ 0, 0 ], c: 0, d: 0 }
      export const [ d, e, ...{f, g, rest3} ] = [0, 0, {f:0,g:0,h:0}]
      let i
      export const j = i = [0, 0]
      export { exists, existsSync } from "https://deno.land/std/fs/exists.ts"
      export * as DenoStdServer from "https://deno.land/std/http/sever.ts"
      export * from "https://deno.land/std/http/sever.ts"
    "#;
		let module = SWC::parse("/app.ts", source, None).expect("could not parse module");
		assert_eq!(
			module.parse_export_names().unwrap(),
			vec![
				"name",
				"version",
				"default",
				"build",
				"dev",
				"Server",
				"a1",
				"a2",
				"b1",
				"b2",
				"c",
				"rest",
				"d",
				"e",
				"f",
				"g",
				"rest3",
				"j",
				"exists",
				"existsSync",
				"DenoStdServer",
				"{https://deno.land/std/http/sever.ts}",
			]
			.into_iter()
			.map(|s| s.to_owned())
			.collect::<Vec<String>>()
		)
	}
}
