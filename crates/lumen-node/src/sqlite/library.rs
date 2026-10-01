//! Select one SQLite implementation for the whole API table. Static addresses avoid OS
//! library discovery on Android; dynamic overrides keep their library alive for all handles.
use super::*;

// libsqlite3-sys omits close_v2 from its Rust bindings, but the bundled C amalgamation
// exports it. Keep Node's deferred-close semantics rather than substituting sqlite3_close.
#[cfg(feature = "bundled-sqlite")]
unsafe extern "C" {
    fn sqlite3_close_v2(db: *mut libsqlite3_sys::sqlite3) -> c_int;
}

pub(super) enum Library {
    Dynamic(DynLib),
    #[cfg(feature = "bundled-sqlite")]
    Bundled,
}

impl Library {
    pub(super) fn load(custom_path: Option<&str>) -> Result<Self, String> {
        #[cfg(all(not(feature = "bundled-sqlite"), target_os = "macos"))]
        let candidates: &[&str] = &["/usr/lib/libsqlite3.dylib", "libsqlite3.dylib"];
        #[cfg(all(not(feature = "bundled-sqlite"), target_os = "linux"))]
        let candidates: &[&str] = &["libsqlite3.so.0", "libsqlite3.so"];
        #[cfg(all(not(feature = "bundled-sqlite"), target_os = "windows"))]
        let candidates: &[&str] = &["winsqlite3.dll", "sqlite3.dll"];
        #[cfg(all(
            not(feature = "bundled-sqlite"),
            not(any(target_os = "macos", target_os = "linux", target_os = "windows"))
        ))]
        let candidates: &[&str] = &["libsqlite3.so.0", "libsqlite3.so", "libsqlite3.dylib"];

        let lib =
            if let Some(path) = custom_path {
                Self::Dynamic(DynLib::open(path).map_err(|error| {
                    format!("could not load custom libsqlite3 '{path}' ({error})")
                })?)
            } else {
                #[cfg(feature = "bundled-sqlite")]
                {
                    Self::Bundled
                }
                #[cfg(not(feature = "bundled-sqlite"))]
                {
                    let mut last_err = String::from("no candidate paths");
                    let found = candidates.iter().find_map(|path| match DynLib::open(path) {
                        Ok(lib) => Some(lib),
                        Err(error) => {
                            last_err = error;
                            None
                        }
                    });
                    Self::Dynamic(found.ok_or_else(|| {
                        format!("could not load the system libsqlite3 ({last_err})")
                    })?)
                }
            };

        Ok(lib)
    }

    pub(super) fn symbol(&self, name: &str) -> Option<*mut c_void> {
        match self {
            Self::Dynamic(lib) => lib.symbol(name),
            #[cfg(feature = "bundled-sqlite")]
            Self::Bundled => {
                // These are addresses of the actual linked SQLite C implementation, not
                // symbol lookups in the executable (which need platform export flags).
                let pointer = match name {
                    "sqlite3_bind_blob" => libsqlite3_sys::sqlite3_bind_blob as *const (),
                    "sqlite3_bind_double" => libsqlite3_sys::sqlite3_bind_double as *const (),
                    "sqlite3_bind_int64" => libsqlite3_sys::sqlite3_bind_int64 as *const (),
                    "sqlite3_bind_null" => libsqlite3_sys::sqlite3_bind_null as *const (),
                    "sqlite3_bind_parameter_count" => {
                        libsqlite3_sys::sqlite3_bind_parameter_count as *const ()
                    }
                    "sqlite3_bind_parameter_index" => {
                        libsqlite3_sys::sqlite3_bind_parameter_index as *const ()
                    }
                    "sqlite3_bind_parameter_name" => {
                        libsqlite3_sys::sqlite3_bind_parameter_name as *const ()
                    }
                    "sqlite3_bind_text" => libsqlite3_sys::sqlite3_bind_text as *const (),
                    "sqlite3_changes" => libsqlite3_sys::sqlite3_changes as *const (),
                    "sqlite3_clear_bindings" => libsqlite3_sys::sqlite3_clear_bindings as *const (),
                    "sqlite3_close_v2" => sqlite3_close_v2 as *const (),
                    "sqlite3_column_blob" => libsqlite3_sys::sqlite3_column_blob as *const (),
                    "sqlite3_column_bytes" => libsqlite3_sys::sqlite3_column_bytes as *const (),
                    "sqlite3_column_count" => libsqlite3_sys::sqlite3_column_count as *const (),
                    "sqlite3_column_database_name" => {
                        libsqlite3_sys::sqlite3_column_database_name as *const ()
                    }
                    "sqlite3_column_decltype" => {
                        libsqlite3_sys::sqlite3_column_decltype as *const ()
                    }
                    "sqlite3_column_double" => libsqlite3_sys::sqlite3_column_double as *const (),
                    "sqlite3_column_int64" => libsqlite3_sys::sqlite3_column_int64 as *const (),
                    "sqlite3_column_name" => libsqlite3_sys::sqlite3_column_name as *const (),
                    "sqlite3_column_origin_name" => {
                        libsqlite3_sys::sqlite3_column_origin_name as *const ()
                    }
                    "sqlite3_column_table_name" => {
                        libsqlite3_sys::sqlite3_column_table_name as *const ()
                    }
                    "sqlite3_column_text" => libsqlite3_sys::sqlite3_column_text as *const (),
                    "sqlite3_column_type" => libsqlite3_sys::sqlite3_column_type as *const (),
                    "sqlite3_compileoption_used" => {
                        libsqlite3_sys::sqlite3_compileoption_used as *const ()
                    }
                    "sqlite3_create_function_v2" => {
                        libsqlite3_sys::sqlite3_create_function_v2 as *const ()
                    }
                    "sqlite3_db_config" => libsqlite3_sys::sqlite3_db_config as *const (),
                    "sqlite3_db_filename" => libsqlite3_sys::sqlite3_db_filename as *const (),
                    "sqlite3_deserialize" => libsqlite3_sys::sqlite3_deserialize as *const (),
                    "sqlite3_enable_load_extension" => {
                        libsqlite3_sys::sqlite3_enable_load_extension as *const ()
                    }
                    "sqlite3_errmsg" => libsqlite3_sys::sqlite3_errmsg as *const (),
                    "sqlite3_exec" => libsqlite3_sys::sqlite3_exec as *const (),
                    "sqlite3_expanded_sql" => libsqlite3_sys::sqlite3_expanded_sql as *const (),
                    "sqlite3_extended_errcode" => {
                        libsqlite3_sys::sqlite3_extended_errcode as *const ()
                    }
                    "sqlite3_file_control" => libsqlite3_sys::sqlite3_file_control as *const (),
                    "sqlite3_finalize" => libsqlite3_sys::sqlite3_finalize as *const (),
                    "sqlite3_free" => libsqlite3_sys::sqlite3_free as *const (),
                    "sqlite3_get_autocommit" => libsqlite3_sys::sqlite3_get_autocommit as *const (),
                    "sqlite3_last_insert_rowid" => {
                        libsqlite3_sys::sqlite3_last_insert_rowid as *const ()
                    }
                    "sqlite3_libversion" => libsqlite3_sys::sqlite3_libversion as *const (),
                    "sqlite3_load_extension" => libsqlite3_sys::sqlite3_load_extension as *const (),
                    "sqlite3_malloc64" => libsqlite3_sys::sqlite3_malloc64 as *const (),
                    "sqlite3_open_v2" => libsqlite3_sys::sqlite3_open_v2 as *const (),
                    "sqlite3_prepare_v2" => libsqlite3_sys::sqlite3_prepare_v2 as *const (),
                    "sqlite3_reset" => libsqlite3_sys::sqlite3_reset as *const (),
                    "sqlite3_result_blob" => libsqlite3_sys::sqlite3_result_blob as *const (),
                    "sqlite3_result_double" => libsqlite3_sys::sqlite3_result_double as *const (),
                    "sqlite3_result_error" => libsqlite3_sys::sqlite3_result_error as *const (),
                    "sqlite3_result_int64" => libsqlite3_sys::sqlite3_result_int64 as *const (),
                    "sqlite3_result_null" => libsqlite3_sys::sqlite3_result_null as *const (),
                    "sqlite3_result_text" => libsqlite3_sys::sqlite3_result_text as *const (),
                    "sqlite3_serialize" => libsqlite3_sys::sqlite3_serialize as *const (),
                    "sqlite3_step" => libsqlite3_sys::sqlite3_step as *const (),
                    "sqlite3_total_changes" => libsqlite3_sys::sqlite3_total_changes as *const (),
                    "sqlite3_user_data" => libsqlite3_sys::sqlite3_user_data as *const (),
                    "sqlite3_value_blob" => libsqlite3_sys::sqlite3_value_blob as *const (),
                    "sqlite3_value_bytes" => libsqlite3_sys::sqlite3_value_bytes as *const (),
                    "sqlite3_value_double" => libsqlite3_sys::sqlite3_value_double as *const (),
                    "sqlite3_value_int64" => libsqlite3_sys::sqlite3_value_int64 as *const (),
                    "sqlite3_value_text" => libsqlite3_sys::sqlite3_value_text as *const (),
                    "sqlite3_value_type" => libsqlite3_sys::sqlite3_value_type as *const (),
                    _ => return None,
                };
                Some(pointer.cast_mut().cast())
            }
        }
    }
}
