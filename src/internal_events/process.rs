use metrics::counter;
use sol_lib::NamedInternalEvent;
use sol_lib::internal_event::{InternalEvent, error_stage, error_type};

use crate::{built_info, config};

#[derive(Debug, NamedInternalEvent)]
pub struct SolStarted;

impl InternalEvent for SolStarted {
    fn emit(self) {
        info!(
            target: "sol",
            message = "Sol has started.",
            debug = built_info::DEBUG,
            version = built_info::PKG_VERSION,
            arch = built_info::TARGET_ARCH,
            revision = built_info::SOL_BUILD_DESC.unwrap_or(""),
        );
        counter!("started_total").increment(1);
    }
}

#[derive(Debug, NamedInternalEvent)]
pub struct SolReloaded<'a> {
    pub config_paths: &'a [config::ConfigPath],
}

impl InternalEvent for SolReloaded<'_> {
    fn emit(self) {
        info!(
            target: "sol",
            message = "Sol has reloaded.",
            path = ?self.config_paths,
            internal_log_rate_limit = false,
        );
        counter!("reloaded_total").increment(1);
    }
}

#[derive(Debug, NamedInternalEvent)]
pub struct SolStopped;

impl InternalEvent for SolStopped {
    fn emit(self) {
        info!(
            target: "sol",
            message = "Sol has stopped.",
        );
        counter!("stopped_total").increment(1);
    }
}

#[derive(Debug, NamedInternalEvent)]
pub struct SolQuit;

impl InternalEvent for SolQuit {
    fn emit(self) {
        info!(
            target: "sol",
            message = "Sol has quit.",
        );
        counter!("quit_total").increment(1);
    }
}

#[derive(Debug, NamedInternalEvent)]
pub struct SolReloadError {
    pub reason: &'static str,
}

impl InternalEvent for SolReloadError {
    fn emit(self) {
        error!(
            message = "Reload was not successful.",
            reason = self.reason,
            error_code = "reload",
            error_type = error_type::CONFIGURATION_FAILED,
            stage = error_stage::PROCESSING,
            internal_log_rate_limit = false,
        );
        counter!(
            "component_errors_total",
            "error_code" => "reload",
            "error_type" => error_type::CONFIGURATION_FAILED,
            "stage" => error_stage::PROCESSING,
            "reason" => self.reason,
        )
        .increment(1);
    }
}

#[derive(Debug, NamedInternalEvent)]
pub struct SolConfigLoadError;

impl InternalEvent for SolConfigLoadError {
    fn emit(self) {
        error!(
            message = "Failed to load config files, reload aborted.",
            error_code = "config_load",
            error_type = error_type::CONFIGURATION_FAILED,
            stage = error_stage::PROCESSING,
            internal_log_rate_limit = false,
        );
        counter!(
            "component_errors_total",
            "error_code" => "config_load",
            "error_type" => error_type::CONFIGURATION_FAILED,
            "stage" => error_stage::PROCESSING,
        )
        .increment(1);
    }
}

#[derive(Debug, NamedInternalEvent)]
pub struct SolRecoveryError;

impl InternalEvent for SolRecoveryError {
    fn emit(self) {
        error!(
            message = "Sol has failed to recover from a failed reload.",
            error_code = "recovery",
            error_type = error_type::CONFIGURATION_FAILED,
            stage = error_stage::PROCESSING,
            internal_log_rate_limit = false,
        );
        counter!(
            "component_errors_total",
            "error_code" => "recovery",
            "error_type" => error_type::CONFIGURATION_FAILED,
            "stage" => error_stage::PROCESSING,
        )
        .increment(1);
    }
}
