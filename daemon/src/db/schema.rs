// @generated automatically by Diesel CLI.

diesel::table! {
    job_runs (id) {
        id -> BigInt,
        lease_id -> Nullable<BigInt>,
        fingerprint -> Text,
        command -> Text,
        cwd -> Text,
        declared_cores -> Float,
        declared_ram_mb -> BigInt,
        declared_gpu_pct -> Float,
        declared_vram_mb -> BigInt,
        declared_est_secs -> BigInt,
        peak_cores -> Float,
        avg_cores -> Float,
        peak_ram_mb -> BigInt,
        avg_ram_mb -> BigInt,
        peak_gpu_pct -> Float,
        avg_gpu_pct -> Float,
        peak_vram_mb -> BigInt,
        avg_vram_mb -> BigInt,
        started_at_ms -> BigInt,
        ended_at_ms -> BigInt,
        noise_avg_cpu_pct -> Nullable<Float>,
        noise_peak_cpu_pct -> Nullable<Float>,
        noise_avg_gpu_pct -> Nullable<Float>,
        noise_peak_gpu_pct -> Nullable<Float>,
        noise_quiet_at_start -> Nullable<Bool>,
        overlapped -> Bool,
    }
}

diesel::table! {
    lease_events (id) {
        id -> BigInt,
        lease_id -> BigInt,
        at_ms -> BigInt,
        from_state -> Nullable<Text>,
        to_state -> Text,
        reason -> Text,
    }
}

diesel::table! {
    leases (id) {
        id -> BigInt,
        session_id -> Text,
        holder_pid -> BigInt,
        holder_pid_start_time -> BigInt,
        kind -> Text,
        state -> Text,
        class -> Nullable<Text>,
        cores -> Float,
        ram_mb -> BigInt,
        gpu_pct -> Float,
        vram_mb -> BigInt,
        est_secs -> BigInt,
        label -> Text,
        requested_at_ms -> BigInt,
        granted_at_ms -> Nullable<BigInt>,
        ended_at_ms -> Nullable<BigInt>,
        expires_at_ms -> BigInt,
        cwd -> Text,
    }
}

diesel::joinable!(job_runs -> leases (lease_id));
diesel::joinable!(lease_events -> leases (lease_id));

diesel::allow_tables_to_appear_in_same_query!(job_runs, lease_events, leases,);
