// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

use super::JsComponentDefinition;
use super::JsDiagnostic;
use i_slint_compiler::langtype::Type;
use itertools::Itertools;
use napi::bindgen_prelude::*;
use napi::{Env, JsValue};
use slint_interpreter::Compiler;
use slint_interpreter::Value;
use smol_str::StrExt;
use std::collections::HashMap;
use std::path::PathBuf;

/// ComponentCompiler is the entry point to the Slint interpreter that can be used
/// to load .slint files or compile them on-the-fly from a string.
#[napi(js_name = "ComponentCompiler")]
pub struct JsComponentCompiler {
    internal: Compiler,
    structs_and_enums: Vec<Type>,
    diagnostics: Vec<slint_interpreter::Diagnostic>,
    /// The options set through the setters, for the compiler that a live preview builds with.
    #[cfg(feature = "live-preview")]
    options: CompilerOptions,
    #[cfg(feature = "live-preview")]
    live_compiler: Option<i_slint_live_preview::live_compiler::LiveCompiler>,
}

/// The options of a `ComponentCompiler` that a compiler on another thread can be set up with.
#[cfg(feature = "live-preview")]
#[derive(Default)]
struct CompilerOptions {
    style: Option<String>,
    include_paths: Option<Vec<PathBuf>>,
    library_paths: Option<HashMap<String, PathBuf>>,
}

#[cfg(feature = "live-preview")]
impl CompilerOptions {
    fn new_compiler(&self) -> Compiler {
        let mut compiler = compiler_from_env();
        if let Some(style) = &self.style {
            compiler.set_style(style.clone());
        }
        if let Some(include_paths) = &self.include_paths {
            compiler.set_include_paths(include_paths.clone());
        }
        if let Some(library_paths) = &self.library_paths {
            compiler.set_library_paths(library_paths.clone());
        }
        compiler
    }
}

/// A compiler with the include and library paths of `SLINT_INCLUDE_PATH` and `SLINT_LIBRARY_PATH`.
fn compiler_from_env() -> Compiler {
    let mut compiler = Compiler::default();
    let include_paths = match std::env::var_os("SLINT_INCLUDE_PATH") {
        Some(paths) => {
            std::env::split_paths(&paths).filter(|path| !path.as_os_str().is_empty()).collect()
        }
        None => Vec::new(),
    };
    let library_paths = match std::env::var_os("SLINT_LIBRARY_PATH") {
        Some(paths) => std::env::split_paths(&paths)
            .filter_map(|entry| {
                entry
                    .to_str()
                    .unwrap_or_default()
                    .split('=')
                    .collect_tuple()
                    .map(|(k, v)| (k.into(), v.into()))
            })
            .collect(),
        None => HashMap::new(),
    };

    // Environment variables rank below the project file, so they bypass the setters.
    let config = compiler.compiler_configuration(i_slint_core::InternalToken);
    config.include_paths = include_paths;
    config.library_paths = library_paths;
    compiler
}

impl JsComponentCompiler {
    fn take_result(
        &mut self,
        result: slint_interpreter::CompilationResult,
    ) -> HashMap<String, JsComponentDefinition> {
        self.structs_and_enums =
            result.structs_and_enums(i_slint_core::InternalToken {}).cloned().collect::<Vec<_>>();
        self.diagnostics = result.diagnostics().collect();
        result.components().map(|c| (c.name().to_owned(), c.into())).collect()
    }
}

#[napi]
impl JsComponentCompiler {
    /// Returns a new ComponentCompiler.
    #[napi(constructor)]
    pub fn new() -> Self {
        Self {
            internal: compiler_from_env(),
            diagnostics: Vec::new(),
            structs_and_enums: vec![],
            #[cfg(feature = "live-preview")]
            options: CompilerOptions::default(),
            #[cfg(feature = "live-preview")]
            live_compiler: None,
        }
    }

    #[napi(setter)]
    pub fn set_include_paths(&mut self, include_paths: Vec<String>) {
        let include_paths: Vec<PathBuf> = include_paths.iter().map(PathBuf::from).collect();
        #[cfg(feature = "live-preview")]
        {
            self.options.include_paths = Some(include_paths.clone());
        }
        self.internal.set_include_paths(include_paths);
    }

    #[napi(getter)]
    pub fn include_paths(&self) -> Vec<String> {
        self.internal
            .include_paths()
            .iter()
            .map(|p| p.to_str().unwrap_or_default().to_string())
            .collect()
    }

    #[napi(setter)]
    pub fn set_library_paths(&mut self, paths: HashMap<String, String>) {
        let mut library_paths = HashMap::new();
        for (key, path) in paths {
            library_paths.insert(key, PathBuf::from(path));
        }

        #[cfg(feature = "live-preview")]
        {
            self.options.library_paths = Some(library_paths.clone());
        }
        self.internal.set_library_paths(library_paths);
    }

    #[napi(getter)]
    pub fn library_paths(&self) -> HashMap<String, String> {
        let mut library_paths = HashMap::new();

        for (key, path) in self.internal.library_paths() {
            library_paths.insert(key.clone(), path.to_str().unwrap_or_default().to_string());
        }

        library_paths
    }

    #[napi(setter)]
    pub fn set_style(&mut self, style: String) {
        #[cfg(feature = "live-preview")]
        {
            self.options.style = Some(style.clone());
        }
        self.internal.set_style(style);
    }

    #[napi(getter)]
    pub fn style(&self) -> Option<String> {
        self.internal.style().cloned()
    }

    #[napi(getter)]
    pub fn diagnostics(&self) -> Vec<JsDiagnostic> {
        self.diagnostics.iter().map(|d| JsDiagnostic::from(d.clone())).collect()
    }

    #[napi(getter)]
    pub fn structs<'a>(&self, env: &'a Env) -> HashMap<String, Unknown<'a>> {
        fn convert_type<'a>(env: &'a Env, ty: &Type) -> Option<(String, Unknown<'a>)> {
            match ty {
                Type::Struct(s) if s.node().is_some() => {
                    let name = s.name.slint_name().unwrap();
                    let struct_instance = crate::to_js_unknown(
                        env,
                        &Value::Struct(slint_interpreter::Struct::from_iter(s.fields.keys().map(
                            |name| {
                                (
                                    name.to_string(),
                                    slint_interpreter::default_value_for_struct_field(s, name),
                                )
                            },
                        ))),
                    );

                    Some((name.to_string(), struct_instance.ok()?))
                }
                _ => None,
            }
        }

        self.structs_and_enums
            .iter()
            .filter_map(|ty| convert_type(env, ty))
            .collect::<HashMap<String, Unknown<'a>>>()
    }

    #[napi(getter)]
    pub fn enums<'a>(&self, env: &'a Env) -> HashMap<String, Unknown<'a>> {
        fn convert_type<'a>(env: &'a Env, ty: &Type) -> Option<(String, Unknown<'a>)> {
            match ty {
                Type::Enumeration(en) => {
                    let mut o = Object::new(env).ok()?;

                    for value in en.values.iter() {
                        // The property name has the dashes replaced, the value is the
                        // spelling the runtime accepts.
                        let str_val = env.create_string(value).ok()?;
                        o.set_named_property(&value.replace_smolstr("-", "_"), str_val).ok()?;
                    }
                    Some((en.name.to_string(), o.into_unknown(env).ok()?))
                }
                _ => None,
            }
        }

        self.structs_and_enums
            .iter()
            .filter_map(|ty| convert_type(env, ty))
            .collect::<HashMap<String, Unknown<'a>>>()
    }

    #[napi(setter)]
    pub fn set_file_loader(
        &mut self,
        env: &Env,
        #[napi(ts_arg_type = "(path: string) => string")] callback: crate::DynFunction<'_>,
    ) -> napi::Result<()> {
        let func_ref = std::rc::Rc::new(callback.create_ref()?);
        let env = *env;

        self.internal.set_file_loader(move |path| {
            let path = PathBuf::from(path);
            let func_ref = func_ref.clone();
            Box::pin({
                async move {
                    let Ok(path_str) = env.create_string(path.display().to_string().as_str())
                    else {
                        return Some(Err(std::io::Error::other(
                            "Node.js: wrong argument for callback file_loader.",
                        )));
                    };

                    let Ok(result) = func_ref
                        .borrow_back(&env)
                        .and_then(|f| f.call(crate::DynArgs(vec![path_str.raw()])))
                    else {
                        return Some(Err(std::io::Error::other(
                            "Node.js: file loader callback failed.",
                        )));
                    };

                    let js_string = result.coerce_to_string();
                    let Ok(js_string) = js_string else {
                        return Some(Err(std::io::Error::other(
                            "Node.js: cannot read return value of file loader callback as js string.",
                        )));
                    };

                    let Ok(utf8_string) = js_string.into_utf8() else {
                        return Some(Err(std::io::Error::other(
                        "Node.js: cannot convert return value of file loader callback into utf8.",
                    )));
                    };

                    if let Ok(str) = utf8_string.as_str() {
                        let string = str.to_string();

                        return Some(Ok(string));
                    };

                    Some(Err(std::io::Error::other(
                        "Node.js: cannot convert return value of file loader callback into string.",
                    )))
                }
            })
        });

        Ok(())
    }

    /// Compile a .slint file into a ComponentDefinition
    ///
    /// Returns the compiled `ComponentDefinition` if there were no errors.
    #[napi]
    pub fn build_from_path(&mut self, path: String) -> HashMap<String, JsComponentDefinition> {
        let result = spin_on::spin_on(self.internal.build_from_path(PathBuf::from(path)));
        self.take_result(result)
    }

    /// @hidden
    /// Like `buildFromPath`, and builds the file again when one of the files it reads changes.
    /// `onRebuild` receives the diagnostics and the components of each new build.
    #[napi(
        ts_args_type = "path: string, onRebuild: (diagnostics: Diagnostic[], definitions: Record<string, ComponentDefinition>) => void"
    )]
    pub fn build_from_path_live(
        &mut self,
        path: String,
        on_rebuild: Function<'_, (), ()>,
    ) -> napi::Result<HashMap<String, JsComponentDefinition>> {
        #[cfg(feature = "live-preview")]
        {
            use napi::threadsafe_function::{ThreadsafeCallContext, ThreadsafeFunctionCallMode};
            let config = self.internal.compiler_configuration(i_slint_core::InternalToken);
            if config.open_import_callback.is_some() {
                return Err(napi::Error::from_reason(
                    "The live preview doesn't support a fileLoader",
                ));
            }
            // Weak, so that watching doesn't keep Node.js running.
            let on_rebuild = on_rebuild
                .build_threadsafe_function::<slint_interpreter::SendableCompilationResult>()
                .callee_handled::<false>()
                .weak::<true>()
                .build_callback(
                    |ctx: ThreadsafeCallContext<slint_interpreter::SendableCompilationResult>| {
                        let result: slint_interpreter::CompilationResult = ctx.value.into();
                        let diagnostics: Vec<JsDiagnostic> =
                            result.diagnostics().map(JsDiagnostic::from).collect();
                        let definitions: HashMap<String, JsComponentDefinition> =
                            result.components().map(|c| (c.name().to_owned(), c.into())).collect();
                        Ok(FnArgs::from((diagnostics, definitions)))
                    },
                )?;
            let options = std::mem::take(&mut self.options);
            let (live_compiler, first_build) =
                i_slint_live_preview::live_compiler::LiveCompiler::start(
                    move || options.new_compiler(),
                    PathBuf::from(path),
                    move |result| {
                        on_rebuild.call(result, ThreadsafeFunctionCallMode::NonBlocking);
                    },
                )
                .map_err(napi::Error::from_reason)?;
            self.live_compiler = Some(live_compiler);
            Ok(self.take_result(first_build.into()))
        }
        #[cfg(not(feature = "live-preview"))]
        {
            let _ = (path, on_rebuild);
            Err(napi::Error::from_reason(
                "Install slint-ui-dev, at the same version as slint-ui, to use the live preview",
            ))
        }
    }

    /// Compile some .slint code into a ComponentDefinition
    #[napi]
    pub fn build_from_source(
        &mut self,
        source_code: String,
        path: String,
    ) -> HashMap<String, JsComponentDefinition> {
        let result =
            spin_on::spin_on(self.internal.build_from_source(source_code, PathBuf::from(path)));
        self.take_result(result)
    }
}
