// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

//! This is an internal module that contains the [`LiveReloadingComponent`] struct.

use crate::live_compiler::LiveCompiler;
use core::cell::RefCell;
use core::task::Waker;
use i_slint_core::api::{ComponentHandle, PlatformError};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

//re-export for the generated code:
pub use crate::live_compiler::poll_ready;
pub use slint_interpreter::{Compiler, ComponentInstance, DefaultTranslationContext, Value};

/// This struct is used to compile and instantiate a component from a .slint file on disk.
/// The file is watched for changes and the component is recompiled and instantiated
pub struct LiveReloadingComponent {
    instance: ComponentInstance,
    // Kept so the FFI can return a stable reference; the window is reused across reloads.
    #[cfg(feature = "ffi")]
    window_adapter: Rc<dyn i_slint_core::window::WindowAdapter>,
    live_compiler: LiveCompiler,
    file_name: PathBuf,
    component_name: Option<String>,
    properties: RefCell<HashMap<String, Value>>,
    callbacks: RefCell<HashMap<String, Rc<dyn Fn(&[Value]) -> Value + 'static>>>,
    post_reload_hook: Option<Box<dyn Fn(&ComponentInstance)>>,
}

/// A build of the compile thread, waiting for the event-loop thread to instantiate it.
#[derive(Default)]
struct PendingBuild {
    result: Option<slint_interpreter::SendableCompilationResult>,
    /// Set while the event-loop future is parked, for the compile thread to wake it.
    waker: Option<Waker>,
}

impl LiveReloadingComponent {
    /// Compile and instantiate a component from the specified .slint file and
    /// component, and watch the file for changes.
    ///
    /// Every compilation, this first one included, runs on a background thread, so
    /// that only the instantiation happens on the current (event-loop) thread. That
    /// thread builds its [`Compiler`] from `compiler_factory`.
    pub fn new(
        compiler_factory: impl FnOnce() -> Compiler + Send + 'static,
        file_name: PathBuf,
        component_name: Option<String>,
    ) -> Result<Rc<RefCell<Self>>, PlatformError> {
        Self::new_impl(compiler_factory, file_name, component_name, None)
    }

    /// Like [`Self::new`], but the component uses `context` instead of the thread's.
    pub fn new_with_context(
        compiler_factory: impl FnOnce() -> Compiler + Send + 'static,
        file_name: PathBuf,
        component_name: Option<String>,
        context: i_slint_core::SlintContext,
    ) -> Result<Rc<RefCell<Self>>, PlatformError> {
        Self::new_impl(compiler_factory, file_name, component_name, Some(context))
    }

    fn new_impl(
        compiler_factory: impl FnOnce() -> Compiler + Send + 'static,
        file_name: PathBuf,
        component_name: Option<String>,
        context: Option<i_slint_core::SlintContext>,
    ) -> Result<Rc<RefCell<Self>>, PlatformError> {
        let pending = Arc::new(Mutex::new(PendingBuild::default()));
        let (live_compiler, first_build) =
            LiveCompiler::start(compiler_factory, file_name.clone(), {
                let pending = pending.clone();
                move |result| {
                    let mut locked = pending.lock().unwrap();
                    locked.result = Some(result);
                    if let Some(waker) = locked.waker.take() {
                        drop(locked);
                        waker.wake();
                    }
                }
            })?;

        let result: slint_interpreter::CompilationResult = first_build.into();
        result.print_diagnostics();
        if result.has_errors() {
            return Err(format!("Could not compile {}", file_name.display()).into());
        }
        let definition = find_component(&result, component_name.as_deref()).ok_or_else(
            || -> PlatformError { format!("No component found in {}", file_name.display()).into() },
        )?;
        let instance = match context {
            Some(context) => definition.create_with_context(context)?,
            None => definition.create()?,
        };
        let self_rc = Rc::new(RefCell::new(Self {
            #[cfg(feature = "ffi")]
            window_adapter: i_slint_core::window::WindowInner::from_pub(instance.window())
                .window_adapter(),
            instance,
            live_compiler,
            file_name,
            component_name,
            properties: Default::default(),
            callbacks: Default::default(),
            post_reload_hook: None,
        }));

        let self_weak = Rc::downgrade(&self_rc);
        // Without an event loop, there's nothing to reload.
        let _ = slint_interpreter::spawn_local(std::future::poll_fn(move |cx| {
            let Some(component) = self_weak.upgrade() else {
                // When the instance is dropped, we can stop this future
                return std::task::Poll::Ready(());
            };
            let reload = {
                let mut locked = pending.lock().unwrap();
                locked.waker = Some(cx.waker().clone());
                locked.result.take()
            };
            if let Some(result) = reload
                && component.borrow_mut().apply_reload(result.into())
            {
                let borrowed = component.borrow();
                borrowed.reload_properties_and_callbacks();
                if let Some(hook) = &borrowed.post_reload_hook {
                    hook(borrowed.instance());
                }
            }
            std::task::Poll::Pending
        }));
        Ok(self_rc)
    }

    /// Swap in a new instance from a result the compile worker produced.
    /// Return false in case of errors.
    fn apply_reload(&mut self, result: slint_interpreter::CompilationResult) -> bool {
        result.print_diagnostics();
        if result.has_errors() {
            return false;
        }

        if let Some(definition) = find_component(&result, self.component_name.as_deref()) {
            match definition.create_with_existing_window(self.instance().window()) {
                Ok(instance) => {
                    self.instance = instance;
                }
                Err(e) => {
                    eprintln!("Error while creating the component: {e}");
                    return false;
                }
            }
        } else {
            eprintln!(
                "Component {} not found",
                self.component_name.as_deref().unwrap_or("<default>")
            );
            return false;
        }
        true
    }
    /// Reload the properties and callbacks after a reload()
    pub fn reload_properties_and_callbacks(&self) {
        // Set the properties
        for (name, value) in self.properties.borrow_mut().iter() {
            if let Some((global, prop)) = name.split_once('.') {
                self.instance()
                    .set_global_property(global, prop, value.clone())
                    .unwrap_or_else(|e| panic!("Cannot set property {name}: {e}"));
            } else {
                self.instance()
                    .set_property(name, value.clone())
                    .unwrap_or_else(|e| panic!("Cannot set property {name}: {e}"));
            }
        }
        for (name, callback) in self.callbacks.borrow_mut().iter() {
            let callback = callback.clone();
            if let Some((global, prop)) = name.split_once('.') {
                self.instance()
                    .set_global_callback(global, prop, move |args| callback(args))
                    .unwrap_or_else(|e| panic!("Cannot set callback {name}: {e}"));
            } else {
                self.instance()
                    .set_callback(name, move |args| callback(args))
                    .unwrap_or_else(|e| panic!("Cannot set callback {name}: {e}"));
            }
        }

        eprintln!(
            "Reloaded component {} from {}",
            self.component_name.as_deref().unwrap_or("<default>"),
            self.file_name.display()
        );
    }

    /// Set a hook that runs after each successful reload, receiving the new instance.
    pub fn set_post_reload_hook(&mut self, hook: impl Fn(&ComponentInstance) + 'static) {
        self.post_reload_hook = Some(Box::new(hook));
    }

    /// Set extra file paths to watch in addition to the .slint file and its imports.
    pub fn set_extra_watch_paths(&mut self, paths: Vec<PathBuf>) {
        self.live_compiler.set_extra_watch_paths(paths);
    }

    /// Return the instance
    pub fn instance(&self) -> &ComponentInstance {
        &self.instance
    }

    /// Set a property and remember its value for when the component is reloaded
    pub fn set_property(&self, name: &str, value: Value) {
        self.properties.borrow_mut().insert(name.into(), value.clone());
        self.instance()
            .set_property(name, value)
            .unwrap_or_else(|e| panic!("Cannot set property {name}: {e}"))
    }

    /// Forward to get_property
    pub fn get_property(&self, name: &str) -> Value {
        self.instance()
            .get_property(name)
            .unwrap_or_else(|e| panic!("Cannot get property {name}: {e}"))
    }

    /// Forward to invoke
    pub fn invoke(&self, name: &str, args: &[Value]) -> Value {
        self.instance()
            .invoke(name, args)
            .unwrap_or_else(|e| panic!("Cannot invoke callback {name}: {e}"))
    }

    /// Forward to set_callback
    pub fn set_callback(&self, name: &str, callback: Rc<dyn Fn(&[Value]) -> Value + 'static>) {
        self.callbacks.borrow_mut().insert(name.into(), callback.clone());
        self.instance()
            .set_callback(name, move |args| callback(args))
            .unwrap_or_else(|e| panic!("Cannot set callback {name}: {e}"));
    }

    /// forward to set_global_property
    pub fn set_global_property(&self, global_name: &str, name: &str, value: Value) {
        self.properties.borrow_mut().insert(format!("{global_name}.{name}"), value.clone());
        self.instance()
            .set_global_property(global_name, name, value)
            .unwrap_or_else(|e| panic!("Cannot set property {global_name}::{name}: {e}"))
    }

    /// forward to get_global_property
    pub fn get_global_property(&self, global_name: &str, name: &str) -> Value {
        self.instance()
            .get_global_property(global_name, name)
            .unwrap_or_else(|e| panic!("Cannot get property {global_name}::{name}: {e}"))
    }

    /// Forward to invoke_global
    pub fn invoke_global(&self, global_name: &str, name: &str, args: &[Value]) -> Value {
        self.instance()
            .invoke_global(global_name, name, args)
            .unwrap_or_else(|e| panic!("Cannot invoke callback {global_name}::{name}: {e}"))
    }

    /// Forward to set_global_callback
    pub fn set_global_callback(
        &self,
        global_name: &str,
        name: &str,
        callback: Rc<dyn Fn(&[Value]) -> Value + 'static>,
    ) {
        self.callbacks.borrow_mut().insert(format!("{global_name}.{name}"), callback.clone());
        self.instance()
            .set_global_callback(global_name, name, move |args| callback(args))
            .unwrap_or_else(|e| panic!("Cannot set callback {global_name}::{name}: {e}"));
    }
}

fn find_component(
    result: &slint_interpreter::CompilationResult,
    component_name: Option<&str>,
) -> Option<slint_interpreter::ComponentDefinition> {
    match component_name {
        Some(name) => result.component(name),
        None => result.components().next(),
    }
}

#[cfg(feature = "ffi")]
mod ffi {
    use super::*;
    use core::ffi::c_void;
    use i_slint_core::window::WindowAdapter;
    use i_slint_core::{SharedString, SharedVector, slice::Slice};
    type LiveReloadingComponentInner = RefCell<LiveReloadingComponent>;

    #[unsafe(no_mangle)]
    /// LibraryPath is an array of string that have in the form `lib=...`
    pub extern "C" fn slint_live_preview_new(
        file_name: Slice<u8>,
        component_name: Slice<u8>,
        include_paths: &SharedVector<SharedString>,
        library_paths: &SharedVector<SharedString>,
        style: Slice<u8>,
        translation_domain: Slice<u8>,
        no_default_translation_context: bool,
        bundled_translations_path: Slice<u8>,
    ) -> *const LiveReloadingComponentInner {
        // Owned, so that the worker thread can build a Compiler of its own from them
        let include_paths: Vec<PathBuf> =
            include_paths.iter().map(|path| PathBuf::from(path.as_str())).collect();
        let library_paths: HashMap<String, PathBuf> = library_paths
            .iter()
            .map(|path| path.as_str().split_once('=').expect("library path must have an '='"))
            .map(|(lib, path)| (lib.into(), PathBuf::from(path)))
            .collect();
        let style = (!style.is_empty()).then(|| std::str::from_utf8(&style).unwrap().to_string());
        let translation_domain = (!translation_domain.is_empty())
            .then(|| std::str::from_utf8(&translation_domain).unwrap().to_string());
        let bundled_translations_path = (!bundled_translations_path.is_empty())
            .then(|| PathBuf::from(std::str::from_utf8(&bundled_translations_path).unwrap()));

        let compiler_factory = move || {
            let mut compiler = Compiler::default();
            compiler.set_include_paths(include_paths.clone());
            compiler.set_library_paths(library_paths.clone());
            if let Some(style) = &style {
                compiler.set_style(style.clone());
            }
            if let Some(translation_domain) = &translation_domain {
                compiler.set_translation_domain(translation_domain.clone());
            }
            if no_default_translation_context {
                compiler.set_default_translation_context(DefaultTranslationContext::None);
            }
            if let Some(bundled_translations_path) = &bundled_translations_path {
                compiler.set_bundled_translations_path(bundled_translations_path.clone());
            }
            compiler
        };
        Rc::into_raw(
            LiveReloadingComponent::new(
                compiler_factory,
                std::path::PathBuf::from(std::str::from_utf8(&file_name).unwrap()),
                Some(std::str::from_utf8(&component_name).unwrap().into()),
            )
            .expect("Creating the component failed"),
        )
    }

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn slint_live_preview_clone(
        component: *const LiveReloadingComponentInner,
    ) {
        unsafe { Rc::increment_strong_count(component) };
    }

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn slint_live_preview_drop(
        component: *const LiveReloadingComponentInner,
    ) {
        unsafe { Rc::decrement_strong_count(component) };
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn slint_live_preview_set_property(
        component: &LiveReloadingComponentInner,
        property: Slice<u8>,
        value: &Value,
    ) {
        let property = std::str::from_utf8(&property).unwrap();
        if let Some((global, prop)) = property.split_once('.') {
            component.borrow_mut().set_global_property(global, prop, value.clone());
        } else {
            component.borrow_mut().set_property(property, value.clone());
        }
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn slint_live_preview_get_property(
        component: &LiveReloadingComponentInner,
        property: Slice<u8>,
    ) -> *mut Value {
        let property = std::str::from_utf8(&property).unwrap();
        let val = if let Some((global, prop)) = property.split_once('.') {
            component.borrow().get_global_property(global, prop)
        } else {
            component.borrow().get_property(property)
        };
        Box::into_raw(Box::new(val))
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn slint_live_preview_invoke(
        component: &LiveReloadingComponentInner,
        callback: Slice<u8>,
        args: Slice<Box<Value>>,
    ) -> *mut Value {
        let callback = std::str::from_utf8(&callback).unwrap();
        let args = args.iter().map(|vb| vb.as_ref().clone()).collect::<Vec<_>>();
        let val = if let Some((global, prop)) = callback.split_once('.') {
            component.borrow().invoke_global(global, prop, &args)
        } else {
            component.borrow().invoke(callback, &args)
        };
        Box::into_raw(Box::new(val))
    }

    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn slint_live_preview_set_callback(
        component: &LiveReloadingComponentInner,
        callback: Slice<u8>,
        callback_handler: extern "C" fn(
            user_data: *mut c_void,
            arg: Slice<Box<Value>>,
        ) -> Box<Value>,
        user_data: *mut c_void,
        drop_user_data: Option<extern "C" fn(*mut c_void)>,
    ) {
        let ud = unsafe {
            slint_interpreter::ffi::CallbackUserData::new(
                user_data,
                drop_user_data,
                callback_handler,
            )
        };
        let handler = Rc::new(move |args: &[Value]| ud.call(args));
        let callback = std::str::from_utf8(&callback).unwrap();
        if let Some((global, prop)) = callback.split_once('.') {
            component.borrow_mut().set_global_callback(global, prop, handler);
        } else {
            component.borrow_mut().set_callback(callback, handler);
        }
    }

    /// Return a borrowed pointer to the component's window adapter (valid while the component lives).
    #[unsafe(no_mangle)]
    pub extern "C" fn slint_live_preview_window(
        component: &LiveReloadingComponentInner,
    ) -> *const i_slint_core::window::ffi::WindowAdapterRcOpaque {
        assert_eq!(
            core::mem::size_of::<Rc<dyn WindowAdapter>>(),
            core::mem::size_of::<i_slint_core::window::ffi::WindowAdapterRcOpaque>()
        );
        let borrow = component.borrow();
        let adapter = &borrow.window_adapter;
        (adapter as *const Rc<dyn WindowAdapter>).cast()
    }
}
