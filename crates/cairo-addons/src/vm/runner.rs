use std::{cell::RefCell, collections::HashMap, rc::Rc};

use crate::vm::{
    layout::PyLayout, maybe_relocatable::PyMaybeRelocatable, program::PyProgram,
    relocatable::PyRelocatable, relocated_trace::PyRelocatedTraceEntry,
    run_resources::PyRunResources,
};
use cairo_vm::{
    hint_processor::builtin_hint_processor::dict_manager::DictManager,
    serde::deserialize_program::Identifier,
    types::{
        builtin_name::BuiltinName,
        relocatable::{MaybeRelocatable, Relocatable},
    },
    vm::{
        errors::vm_exception::VmException,
        runners::{builtin_runner::BuiltinRunner, cairo_runner::CairoRunner as RustCairoRunner},
        security::verify_secure_runner,
    },
};
use num_traits::Zero;
use polars::prelude::*;
use pyo3::{
    prelude::*,
    types::{IntoPyDict, PyDict},
};
use pyo3_polars::PyDataFrame;
use std::ffi::CString;

use super::{
    dict_manager::PyDictManager, hints::HintProcessor, memory_segments::PyMemorySegmentManager,
};

#[pyclass(name = "CairoRunner", unsendable)]
pub struct PyCairoRunner {
    inner: RustCairoRunner,
    allow_missing_builtins: bool,
    builtins: Vec<BuiltinName>,
    enable_pythonic_hints: bool,
}

#[pymethods]
impl PyCairoRunner {
    /// Initialize the runner with the given program and identifiers.
    /// # Arguments
    /// * `program` - The _rust_ program to run.
    /// * `py_identifiers` - The _pythonic_ identifiers for this program.
    /// * `layout` - The layout to use for the runner.
    /// * `proof_mode` - Whether to run in proof mode.
    /// * `allow_missing_builtins` - Whether to allow missing builtins.
    #[new]
    #[pyo3(signature = (program, py_identifiers=None, layout=None, proof_mode=false, allow_missing_builtins=false, enable_pythonic_hints=false))]
    fn new(
        program: &PyProgram,
        py_identifiers: Option<PyObject>,
        layout: Option<PyLayout>,
        proof_mode: bool,
        allow_missing_builtins: bool,
        enable_pythonic_hints: bool,
    ) -> PyResult<Self> {
        let layout = layout.unwrap_or_default().into_layout_name()?;

        let mut inner = RustCairoRunner::new(
            &program.inner,
            layout,
            None, // dynamic_layout_params
            proof_mode,
            true, // trace_enabled
            true, // disable_trace_padding
        )
        .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(e.to_string()))?;

        let dict_manager = DictManager::new();
        inner.exec_scopes.insert_value("dict_manager", Rc::new(RefCell::new(dict_manager)));

        if !enable_pythonic_hints || !cfg!(feature = "pythonic-hints") {
            return Ok(Self {
                inner,
                allow_missing_builtins,
                builtins: program.inner.iter_builtins().copied().collect(),
                enable_pythonic_hints,
            });
        }

        // Add context variables required for pythonic hint execution

        let identifiers = program
            .inner
            .iter_identifiers()
            .map(|(name, identifier)| (name.to_string(), identifier.clone()))
            .collect::<HashMap<String, Identifier>>();

        // Insert the _rust_ program_identifiers in the exec_scopes, so that we're able to pull
        // identifier data when executing hints to build VmConsts.
        inner.exec_scopes.insert_value("__program_identifiers__", identifiers);

        // Initialize a python context object that will be accessible throughout the execution of
        // all hints.
        // This enables us to directly use the Python identifiers passed in, avoiding the need to
        // serialize and deserialize the program JSON.
        Python::with_gil(|py| {
            let context = PyDict::new(py);

            if let Some(py_identifiers) = py_identifiers {
                // Store the Python identifiers directly in the context
                context.set_item("py_identifiers", py_identifiers).map_err(|e| {
                    PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(e.to_string())
                })?;
            }

            // Import and run the initialization code from the injected module
            let setup_code = r#"
try:
    from cairo_addons.hints.injected import prepare_context
    prepare_context(lambda: globals())
except Exception as e:
    print(f"Warning: Error during initialization: {e}")
"#;

            // Run the initialization code
            py.run(&CString::new(setup_code)?, Some(&context), None).map_err(|e| {
                PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(format!(
                    "Failed to initialize Python globals: {}",
                    e
                ))
            })?;

            // Store the context object, modified in the initialization code, in the exec_scopes
            // to access it throughout the execution of hints
            let unbounded_context: Py<PyDict> = context.into_py_dict(py)?.into();
            inner.exec_scopes.insert_value("__context__", unbounded_context);
            Ok::<(), PyErr>(())
        })?;

        Ok(Self {
            inner,
            allow_missing_builtins,
            builtins: program.inner.iter_builtins().copied().collect(),
            enable_pythonic_hints,
        })
    }

    /// Initialize the runner program_base, execution_base and builtins segments.
    pub fn initialize_segments(&mut self) -> PyResult<()> {
        self.inner
            .initialize_builtins(self.allow_missing_builtins)
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(e.to_string()))?;
        self.inner.initialize_segments(None);

        Ok(())
    }

    /// Initialize the runner with the given stack and entrypoint offset.
    #[pyo3(signature = (stack, entrypoint, ordered_builtins=None))]
    pub fn initialize_vm(
        &mut self,
        stack: Vec<PyMaybeRelocatable>,
        entrypoint: usize,
        ordered_builtins: Option<Vec<String>>,
    ) -> PyResult<PyRelocatable> {
        let initial_stack = self.builtins_stack(ordered_builtins)?;
        let stack = initial_stack.into_iter().chain(stack.into_iter().map(|x| x.into())).collect();

        let return_fp = self.inner.vm.add_memory_segment();
        let end = self
            .inner
            .initialize_function_entrypoint(entrypoint, stack, return_fp.into())
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(e.to_string()))?;

        for builtin_runner in self.inner.vm.builtin_runners.iter_mut() {
            if let BuiltinRunner::Mod(runner) = builtin_runner {
                runner.initialize_zero_segment(&mut self.inner.vm.segments);
            }
        }
        self.inner
            .initialize_vm()
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(e.to_string()))?;

        Ok(PyRelocatable { inner: end })
    }

    #[getter]
    fn program_base(&self) -> Option<PyRelocatable> {
        self.inner.program_base.map(|x| PyRelocatable { inner: x })
    }

    #[getter]
    fn execution_base(&self) -> Option<PyRelocatable> {
        // execution_base is not stored but we know it's created right after program_base
        // during initialize_segments(None), so we can derive it by incrementing the segment_index
        self.inner.program_base.map(|x| PyRelocatable {
            inner: Relocatable { segment_index: x.segment_index + 1, offset: 0 },
        })
    }

    #[getter]
    fn ap(&self) -> PyRelocatable {
        PyRelocatable { inner: self.inner.vm.get_ap() }
    }

    #[getter]
    fn fp(&self) -> PyRelocatable {
        PyRelocatable { inner: self.inner.vm.get_fp() }
    }

    #[getter]
    fn pc(&self) -> PyRelocatable {
        PyRelocatable { inner: self.inner.vm.get_pc() }
    }

    #[getter]
    fn segments(&mut self) -> PyMemorySegmentManager {
        PyMemorySegmentManager { vm: &mut self.inner.vm }
    }

    #[getter]
    fn dict_manager(&self) -> PyResult<PyDictManager> {
        let dict_manager = self
            .inner
            .exec_scopes
            .get_dict_manager()
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(e.to_string()))?;
        Ok(PyDictManager { inner: dict_manager })
    }

    #[pyo3(signature = (address, resources))]
    fn run_until_pc(&mut self, address: PyRelocatable, resources: PyRunResources) -> PyResult<()> {
        let mut hint_processor = if self.enable_pythonic_hints {
            HintProcessor::default()
                .with_run_resources(resources.inner)
                .with_dynamic_python_hints()
                .build()
        } else {
            HintProcessor::default().with_run_resources(resources.inner).build()
        };

        self.inner
            .run_until_pc(address.inner, &mut hint_processor)
            .map_err(|e| VmException::from_vm_error(&self.inner, e))
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(e.to_string()))?;

        self.inner
            .end_run(false, false, &mut hint_processor)
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(e.to_string()))?;
        Ok(())
    }

    fn verify_auto_deductions(&mut self) -> PyResult<()> {
        self.inner
            .vm
            .verify_auto_deductions()
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(e.to_string()))?;

        Ok(())
    }

    fn read_return_values(&mut self, offset: usize) -> PyResult<()> {
        self._read_return_values(offset)?;

        Ok(())
    }

    fn verify_secure_runner(&mut self) -> PyResult<()> {
        verify_secure_runner(&self.inner, true, None)
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(e.to_string()))?;

        Ok(())
    }

    fn verify_and_relocate(&mut self, offset: usize) -> PyResult<()> {
        self.verify_auto_deductions()?;
        self.read_return_values(offset)?;
        self.verify_secure_runner()?;
        self.relocate()?;
        Ok(())
    }

    fn relocate(&mut self) -> PyResult<()> {
        self.inner
            .relocate(true)
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(e.to_string()))?;

        Ok(())
    }

    #[getter]
    fn relocated_trace(&self) -> PyResult<Vec<PyRelocatedTraceEntry>> {
        Ok(self
            .inner
            .relocated_trace
            .clone()
            .unwrap_or_default()
            .into_iter()
            .map(PyRelocatedTraceEntry::from)
            .collect())
    }

    #[getter]
    fn trace_df(&self) -> PyResult<PyDataFrame> {
        let relocated_trace = self.inner.relocated_trace.clone().unwrap_or_default();
        let trace_len = relocated_trace.len();
        let mut pc_values = Vec::with_capacity(trace_len);
        let mut ap_values = Vec::with_capacity(trace_len);
        let mut fp_values = Vec::with_capacity(trace_len);

        for entry in relocated_trace.iter() {
            pc_values.push(entry.pc as u64);
            ap_values.push(entry.ap as u64);
            fp_values.push(entry.fp as u64);
        }

        let df = df!("pc" => pc_values, "ap" => ap_values, "fp" => fp_values)
            .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(e.to_string()))?;

        Ok(PyDataFrame(df))
    }
}

impl PyCairoRunner {
    fn builtins_stack(
        &mut self,
        ordered_builtins: Option<Vec<String>>,
    ) -> PyResult<Vec<MaybeRelocatable>> {
        let mut stack = Vec::new();
        let builtin_runners =
            self.inner.vm.builtin_runners.iter().map(|b| (b.name(), b)).collect::<HashMap<_, _>>();

        if let Some(names) = ordered_builtins {
            self.builtins = names
                .iter()
                .map(|name| {
                    BuiltinName::from_str_with_suffix(name).ok_or_else(|| {
                        PyErr::new::<pyo3::exceptions::PyValueError, _>(format!(
                            "Invalid builtin name: {}",
                            name
                        ))
                    })
                })
                .collect::<PyResult<Vec<_>>>()?;
        };
        for builtin_name in self.builtins.iter() {
            if let Some(builtin_runner) = builtin_runners.get(builtin_name) {
                stack.append(&mut builtin_runner.initial_stack());
            } else {
                return Err(PyErr::new::<pyo3::exceptions::PyValueError, _>(format!(
                    "Builtin runner {} not found",
                    builtin_name
                )));
            }
        }
        Ok(stack)
    }

    /// Mainly like `CairoRunner::read_return_values` but with an `offset` parameter and some checks
    /// that I needed to remove.
    fn _read_return_values(&mut self, offset: usize) -> PyResult<()> {
        let mut pointer = (self.inner.vm.get_ap() - offset).unwrap();
        for builtin_name in self.builtins.iter().rev() {
            if let Some(builtin_runner) =
                self.inner.vm.builtin_runners.iter_mut().find(|b| b.name() == *builtin_name)
            {
                let new_pointer =
                    builtin_runner.final_stack(&self.inner.vm.segments, pointer).map_err(|e| {
                        PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(e.to_string())
                    })?;
                pointer = new_pointer;
            } else {
                if !self.allow_missing_builtins {
                    return Err(PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(format!(
                        "Missing builtin: {}",
                        builtin_name
                    )));
                }
                pointer.offset = pointer.offset.saturating_sub(1);

                if !self
                    .inner
                    .vm
                    .get_integer(pointer)
                    .map_err(|e| PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(e.to_string()))?
                    .is_zero()
                {
                    return Err(PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(format!(
                        "Missing builtin stop ptr not zero: {}",
                        builtin_name
                    )));
                }
            }
        }
        Ok(())
    }
}
