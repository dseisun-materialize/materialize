// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

// BEGIN LINT CONFIG
// DO NOT EDIT. Automatically generated by bin/gen-lints.
// Have complaints about the noise? See the note in misc/python/materialize/cli/gen-lints.py first.
#![allow(clippy::style)]
#![allow(clippy::complexity)]
#![allow(clippy::large_enum_variant)]
#![allow(clippy::mutable_key_type)]
#![allow(clippy::stable_sort_primitive)]
#![allow(clippy::map_entry)]
#![allow(clippy::box_default)]
#![warn(clippy::bool_comparison)]
#![warn(clippy::clone_on_ref_ptr)]
#![warn(clippy::no_effect)]
#![warn(clippy::unnecessary_unwrap)]
#![warn(clippy::dbg_macro)]
#![warn(clippy::todo)]
#![warn(clippy::wildcard_dependencies)]
#![warn(clippy::zero_prefixed_literal)]
#![warn(clippy::borrowed_box)]
#![warn(clippy::deref_addrof)]
#![warn(clippy::double_must_use)]
#![warn(clippy::double_parens)]
#![warn(clippy::extra_unused_lifetimes)]
#![warn(clippy::needless_borrow)]
#![warn(clippy::needless_question_mark)]
#![warn(clippy::needless_return)]
#![warn(clippy::redundant_pattern)]
#![warn(clippy::redundant_slicing)]
#![warn(clippy::redundant_static_lifetimes)]
#![warn(clippy::single_component_path_imports)]
#![warn(clippy::unnecessary_cast)]
#![warn(clippy::useless_asref)]
#![warn(clippy::useless_conversion)]
#![warn(clippy::builtin_type_shadow)]
#![warn(clippy::duplicate_underscore_argument)]
#![warn(clippy::double_neg)]
#![warn(clippy::unnecessary_mut_passed)]
#![warn(clippy::wildcard_in_or_patterns)]
#![warn(clippy::collapsible_if)]
#![warn(clippy::collapsible_else_if)]
#![warn(clippy::crosspointer_transmute)]
#![warn(clippy::excessive_precision)]
#![warn(clippy::overflow_check_conditional)]
#![warn(clippy::as_conversions)]
#![warn(clippy::match_overlapping_arm)]
#![warn(clippy::zero_divided_by_zero)]
#![warn(clippy::must_use_unit)]
#![warn(clippy::suspicious_assignment_formatting)]
#![warn(clippy::suspicious_else_formatting)]
#![warn(clippy::suspicious_unary_op_formatting)]
#![warn(clippy::mut_mutex_lock)]
#![warn(clippy::print_literal)]
#![warn(clippy::same_item_push)]
#![warn(clippy::useless_format)]
#![warn(clippy::write_literal)]
#![warn(clippy::redundant_closure)]
#![warn(clippy::redundant_closure_call)]
#![warn(clippy::unnecessary_lazy_evaluations)]
#![warn(clippy::partialeq_ne_impl)]
#![warn(clippy::redundant_field_names)]
#![warn(clippy::transmutes_expressible_as_ptr_casts)]
#![warn(clippy::unused_async)]
#![warn(clippy::disallowed_methods)]
#![warn(clippy::disallowed_macros)]
#![warn(clippy::disallowed_types)]
#![warn(clippy::from_over_into)]
// END LINT CONFIG

/// This code is built to load test Kinesis sources.
///
/// Essentially, it:
///     - Generates some amount of data (total_records). Right now, the data are just
///       random strings converted to bytes.
///     - Pushes the generated data to the target Kinesis stream (at a rate of records_per_second).
///     - Creates a source from the Kinesis stream. Create a materialized view of the count
///       of records from the stream.
///
/// The test will end and is considered successful iff all records are pushed to
/// Kinesis, all records are accounted for in materialized, AND the performance seems
/// reasonable.
///
/// To evaluate overall performance, we use the latency metrics in the Grafana dashboard.
/// In general, the server side latencies should be low and consistent over time. Additionally,
/// "Time behind external source," which indicates our lag behind the tip of the Kinesis
/// stream, should not drift over time. (These measurements should become more concrete as
/// we get more experience running this test).
use std::io;

use anyhow::Context;
use rand::Rng;
use tracing::info;
use tracing_subscriber::filter::EnvFilter;

use mz_ore::cli::{self, CliConfig};
use mz_ore::task;
use mz_test_util::mz_client;

mod kinesis;
mod mz;

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("ERROR: {:#?}", e);
        std::process::exit(1);
    }
}

async fn run() -> Result<(), anyhow::Error> {
    let timer = std::time::Instant::now();
    let args: Args = cli::parse_args(CliConfig::default());

    tracing_subscriber::fmt()
        .with_env_filter(args.log_filter)
        .with_writer(io::stderr)
        .init();

    // Initialize and log test variables.
    let seed: u32 = rand::thread_rng().gen();
    let stream_name = format!("{}-{}", args.stream_prefix, seed);

    // todo: make queries per second configurable. (requires mz_client changes)
    info!("Starting kinesis load test with mzd={}:{} \
               stream={} shard_count={} total_records={} records_per_second={} queries_per_second={}",
    args.materialized_host, args.materialized_port, &stream_name, args.shard_count, args.total_records, args.records_per_second, 1);

    // Initialize test resources in Kinesis.
    let config = aws_config::load_from_env().await;
    let kinesis_client = aws_sdk_kinesis::Client::new(&config);

    let stream_arn =
        kinesis::create_stream(&kinesis_client, &stream_name, args.shard_count).await?;
    info!("Created Kinesis stream {}", stream_name);

    // Push records to Kinesis.
    let kinesis_task = task::spawn(|| "kinesis_task", {
        let kinesis_client_clone = kinesis_client.clone();
        let stream_name_clone = stream_name.clone();
        let total_records = args.total_records;
        let records_per_second = args.records_per_second;
        async move {
            kinesis::generate_and_put_records(
                kinesis_client_clone,
                &stream_name_clone,
                total_records,
                records_per_second,
            )
            .await
        }
    });

    // Initialize connection to materialized instance.
    let client = mz_client::client(&args.materialized_host, args.materialized_port)
        .await
        .context("creating postgres client")?;

    // Create Kinesis source and materialized view.
    mz::create_source_and_views(&client, &config, stream_arn).await?;
    info!("Created source and materialized views");

    // Query materialized view for all pushed Kinesis records.
    let materialize_task = task::spawn(|| "kinesis_mz_verify", {
        let total_records = args.total_records;
        async move { mz::query_materialized_view_until(&client, "foo_count", total_records).await }
    });

    let (kinesis_result, materialize_result) = futures::join!(kinesis_task, materialize_task);

    kinesis::delete_stream(&kinesis_client, &stream_name).await?;

    kinesis_result?.context("kinesis thread failed")?;
    materialize_result.context("materialize thread failed")??;
    info!(
        "Completed load test in {} milliseconds",
        timer.elapsed().as_millis()
    );

    Ok(())
}

#[derive(Debug, clap::Parser)]
pub struct Args {
    /// The materialized host
    #[clap(long, default_value = "materialized")]
    pub materialized_host: String,

    /// The materialized port
    #[clap(long, default_value = "6875")]
    pub materialized_port: u16,

    /// The number of shards in the Kinesis stream
    #[clap(long, default_value = "50")]
    pub shard_count: i32,

    /// The total number of records to create
    #[clap(long, default_value = "150000000")]
    pub total_records: u64,

    /// The number of records to put to the Kinesis stream per second
    #[clap(long, default_value = "2000")]
    pub records_per_second: u64,

    /// The name of the stream to use, will always have a nonce
    #[clap(long, default_value = "testdrive-perf-kinesis")]
    pub stream_prefix: String,

    /// Which log messages to emit.
    ///
    /// See materialized's `--log-filter` option for details.
    #[clap(long, value_name = "FILTER", default_value = "perf-kinesis=debug,info")]
    pub log_filter: EnvFilter,
}
