// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! Benchmark derived from TPC-H. This is not an official TPC-H benchmark.

use std::{
    fs::{self, File},
    io::Write,
    iter::Iterator,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Instant, SystemTime},
};

use datafusion::datasource::{MemTable, TableProvider};
use datafusion::error::{DataFusionError, Result};
use datafusion::parquet::basic::Compression;
use datafusion::parquet::file::properties::WriterProperties;
use datafusion::physical_plan::display::DisplayableExecutionPlan;
use datafusion::physical_plan::{collect, displayable};
use datafusion::prelude::*;
use datafusion::{
    arrow::datatypes::{DataType, Field, Schema},
    datasource::file_format::{csv::CsvFormat, FileFormat},
    DATAFUSION_VERSION,
};
use datafusion::{
    arrow::record_batch::RecordBatch, datasource::file_format::parquet::ParquetFormat,
};
use datafusion::{
    arrow::util::pretty,
    datasource::listing::{ListingOptions, ListingTable, ListingTableConfig},
};

use datafusion::datasource::file_format::csv::DEFAULT_CSV_EXTENSION;
use datafusion::datasource::file_format::parquet::DEFAULT_PARQUET_EXTENSION;
use datafusion::datasource::listing::ListingTableUrl;
use datafusion::execution::context::SessionState;
use serde::Serialize;
use structopt::StructOpt;

#[cfg(feature = "snmalloc")]
#[global_allocator]
static ALLOC: snmalloc_rs::SnMalloc = snmalloc_rs::SnMalloc;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Debug, StructOpt, Clone)]
struct DataFusionBenchmarkOpt {
    /// Query number
    #[structopt(short, long)]
    query: usize,

    /// Activate debug mode to see query results
    #[structopt(short, long)]
    debug: bool,

    /// Number of iterations of each test run
    #[structopt(short = "i", long = "iterations", default_value = "3")]
    iterations: usize,

    /// Number of partitions to process in parallel
    #[structopt(short = "n", long = "partitions", default_value = "2")]
    partitions: usize,

    /// Batch size when reading CSV or Parquet files
    #[structopt(short = "s", long = "batch-size", default_value = "8192")]
    batch_size: usize,

    /// Path to data files
    #[structopt(parse(from_os_str), required = true, short = "p", long = "path")]
    path: PathBuf,

    /// File format: `csv` or `parquet`
    #[structopt(short = "f", long = "format", default_value = "csv")]
    file_format: String,

    /// Load the data into a MemTable before executing the query
    #[structopt(short = "m", long = "mem-table")]
    mem_table: bool,

    /// Path to output directory where JSON summary file should be written to
    #[structopt(parse(from_os_str), short = "o", long = "output")]
    output_path: Option<PathBuf>,

    /// Whether to disable collection of statistics (and cost based optimizations) or not.
    #[structopt(short = "S", long = "disable-statistics")]
    disable_statistics: bool,
}

#[derive(Debug, StructOpt)]
struct ConvertOpt {
    /// Path to csv files
    #[structopt(parse(from_os_str), required = true, short = "i", long = "input")]
    input_path: PathBuf,

    /// Output path
    #[structopt(parse(from_os_str), required = true, short = "o", long = "output")]
    output_path: PathBuf,

    /// Output file format: `csv` or `parquet`
    #[structopt(short = "f", long = "format")]
    file_format: String,

    /// Compression to use when writing Parquet files
    #[structopt(short = "c", long = "compression", default_value = "zstd")]
    compression: String,

    /// Number of partitions to produce
    #[structopt(short = "n", long = "partitions", default_value = "1")]
    partitions: usize,

    /// Batch size when reading CSV or Parquet files
    #[structopt(short = "s", long = "batch-size", default_value = "8192")]
    batch_size: usize,
}

#[derive(Debug, StructOpt)]
#[structopt(about = "benchmark command")]
enum BenchmarkSubCommandOpt {
    #[structopt(name = "datafusion")]
    DataFusionBenchmark(DataFusionBenchmarkOpt),
}

#[derive(Debug, StructOpt)]
#[structopt(name = "TPC-H", about = "TPC-H Benchmarks.")]
enum TpchOpt {
    Benchmark(BenchmarkSubCommandOpt),
    Convert(ConvertOpt),
}

const TABLES: &[&str] = &[
    "part", "supplier", "partsupp", "customer", "orders", "lineitem", "nation", "region",
];

#[tokio::main]
async fn main() -> Result<()> {
    use BenchmarkSubCommandOpt::*;

    env_logger::init();
    match TpchOpt::from_args() {
        TpchOpt::Benchmark(DataFusionBenchmark(opt)) => {
            benchmark_datafusion(opt).await.map(|_| ())
        }
        TpchOpt::Convert(opt) => convert_tbl(opt).await,
    }
}

#[allow(clippy::await_holding_lock)]
async fn benchmark_datafusion(opt: DataFusionBenchmarkOpt) -> Result<Vec<RecordBatch>> {
    println!("Running benchmarks with the following options: {:?}", opt);
    let mut benchmark_run = BenchmarkRun::new(opt.query);
    let config = SessionConfig::new()
        .with_target_partitions(opt.partitions)
        .with_batch_size(opt.batch_size)
        .with_collect_statistics(!opt.disable_statistics);
    let ctx = SessionContext::with_config(config);

    // register tables
    for table in TABLES {
        let table_provider = {
            let mut session_state = ctx.state.write();
            get_table(
                &mut session_state,
                opt.path.to_str().unwrap(),
                table,
                opt.file_format.as_str(),
                opt.partitions,
            )
            .await?
        };

        if opt.mem_table {
            println!("Loading table '{}' into memory", table);
            let start = Instant::now();
            let memtable =
                MemTable::load(table_provider, Some(opt.partitions), &ctx.state())
                    .await?;
            println!(
                "Loaded table '{}' into memory in {} ms",
                table,
                start.elapsed().as_millis()
            );
            ctx.register_table(*table, Arc::new(memtable))?;
        } else {
            ctx.register_table(*table, table_provider)?;
        }
    }

    let mut millis = vec![];
    // run benchmark
    let mut result: Vec<RecordBatch> = Vec::with_capacity(1);
    for i in 0..opt.iterations {
        let start = Instant::now();

        let sql = &get_query_sql(opt.query)?;

        // query 15 is special, with 3 statements. the second statement is the one from which we
        // want to capture the results
        if opt.query == 15 {
            for (n, query) in sql.iter().enumerate() {
                if n == 1 {
                    result = execute_query(&ctx, query, opt.debug).await?;
                } else {
                    execute_query(&ctx, query, opt.debug).await?;
                }
            }
        } else {
            for query in sql {
                result = execute_query(&ctx, query, opt.debug).await?;
            }
        }

        let elapsed = start.elapsed().as_secs_f64() * 1000.0;
        millis.push(elapsed as f64);
        let row_count = result.iter().map(|b| b.num_rows()).sum();
        println!(
            "Query {} iteration {} took {:.1} ms and returned {} rows",
            opt.query, i, elapsed, row_count
        );
        benchmark_run.add_result(elapsed, row_count);
    }

    let avg = millis.iter().sum::<f64>() / millis.len() as f64;
    println!("Query {} avg time: {:.2} ms", opt.query, avg);

    if let Some(path) = &opt.output_path {
        write_summary_json(&mut benchmark_run, path)?;
    }

    Ok(result)
}

fn write_summary_json(benchmark_run: &mut BenchmarkRun, path: &Path) -> Result<()> {
    let json =
        serde_json::to_string_pretty(&benchmark_run).expect("summary is serializable");
    let filename = format!(
        "tpch-q{}-{}.json",
        benchmark_run.query, benchmark_run.start_time
    );
    let path = path.join(filename);
    println!(
        "Writing summary file to {}",
        path.as_os_str().to_str().unwrap()
    );
    let mut file = File::create(path)?;
    file.write_all(json.as_bytes())?;
    Ok(())
}

/// Get the SQL statements from the specified query file
fn get_query_sql(query: usize) -> Result<Vec<String>> {
    if query > 0 && query < 23 {
        let possibilities = vec![
            format!("queries/q{}.sql", query),
            format!("benchmarks/queries/q{}.sql", query),
        ];
        let mut errors = vec![];
        for filename in possibilities {
            match fs::read_to_string(&filename) {
                Ok(contents) => {
                    return Ok(contents
                        .split(';')
                        .map(|s| s.trim())
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string())
                        .collect());
                }
                Err(e) => errors.push(format!("{}: {}", filename, e)),
            };
        }
        Err(DataFusionError::Plan(format!(
            "invalid query. Could not find query: {:?}",
            errors
        )))
    } else {
        Err(DataFusionError::Plan(
            "invalid query. Expected value between 1 and 22".to_owned(),
        ))
    }
}

async fn execute_query(
    ctx: &SessionContext,
    sql: &str,
    debug: bool,
) -> Result<Vec<RecordBatch>> {
    let plan = ctx.sql(sql).await?;
    let plan = plan.to_logical_plan()?;

    if debug {
        println!("=== Logical plan ===\n{:?}\n", plan);
    }

    if debug {
        let plan = ctx.optimize(&plan)?;
        println!("=== Optimized logical plan ===\n{:?}\n", plan);
    }
    let physical_plan = ctx.create_physical_plan(&plan).await?;
    if debug {
        println!(
            "=== Physical plan ===\n{}\n",
            displayable(physical_plan.as_ref()).indent()
        );
    }
    let task_ctx = ctx.task_ctx();
    let result = collect(physical_plan.clone(), task_ctx).await?;
    if debug {
        println!(
            "=== Physical plan with metrics ===\n{}\n",
            DisplayableExecutionPlan::with_metrics(physical_plan.as_ref()).indent()
        );
        pretty::print_batches(&result)?;
    }
    Ok(result)
}

async fn convert_tbl(opt: ConvertOpt) -> Result<()> {
    let output_root_path = Path::new(&opt.output_path);
    for table in TABLES {
        let start = Instant::now();
        let schema = get_schema(table);

        let input_path = format!("{}/{}.tbl", opt.input_path.to_str().unwrap(), table);
        let options = CsvReadOptions::new()
            .schema(&schema)
            .has_header(false)
            .delimiter(b'|')
            .file_extension(".tbl");

        let config = SessionConfig::new().with_batch_size(opt.batch_size);
        let ctx = SessionContext::with_config(config);

        // build plan to read the TBL file
        let mut csv = ctx.read_csv(&input_path, options).await?;

        // optionally, repartition the file
        if opt.partitions > 1 {
            csv = csv.repartition(Partitioning::RoundRobinBatch(opt.partitions))?
        }

        // create the physical plan
        let csv = csv.to_logical_plan()?;
        let csv = ctx.create_physical_plan(&csv).await?;

        let output_path = output_root_path.join(table);
        let output_path = output_path.to_str().unwrap().to_owned();

        println!(
            "Converting '{}' to {} files in directory '{}'",
            &input_path, &opt.file_format, &output_path
        );
        match opt.file_format.as_str() {
            "csv" => ctx.write_csv(csv, output_path).await?,
            "parquet" => {
                let compression = match opt.compression.as_str() {
                    "none" => Compression::UNCOMPRESSED,
                    "snappy" => Compression::SNAPPY,
                    "brotli" => Compression::BROTLI,
                    "gzip" => Compression::GZIP,
                    "lz4" => Compression::LZ4,
                    "lz0" => Compression::LZO,
                    "zstd" => Compression::ZSTD,
                    other => {
                        return Err(DataFusionError::NotImplemented(format!(
                            "Invalid compression format: {}",
                            other
                        )));
                    }
                };
                let props = WriterProperties::builder()
                    .set_compression(compression)
                    .build();
                ctx.write_parquet(csv, output_path, Some(props)).await?
            }
            other => {
                return Err(DataFusionError::NotImplemented(format!(
                    "Invalid output format: {}",
                    other
                )));
            }
        }
        println!("Conversion completed in {} ms", start.elapsed().as_millis());
    }

    Ok(())
}

async fn get_table(
    ctx: &mut SessionState,
    path: &str,
    table: &str,
    table_format: &str,
    target_partitions: usize,
) -> Result<Arc<dyn TableProvider>> {
    let (format, path, extension): (Arc<dyn FileFormat>, String, &'static str) =
        match table_format {
            // dbgen creates .tbl ('|' delimited) files without header
            "tbl" => {
                let path = format!("{}/{}.tbl", path, table);

                let format = CsvFormat::default()
                    .with_delimiter(b'|')
                    .with_has_header(false);

                (Arc::new(format), path, ".tbl")
            }
            "csv" => {
                let path = format!("{}/{}", path, table);
                let format = CsvFormat::default()
                    .with_delimiter(b',')
                    .with_has_header(true);

                (Arc::new(format), path, DEFAULT_CSV_EXTENSION)
            }
            "parquet" => {
                let path = format!("{}/{}", path, table);
                let format = ParquetFormat::default().with_enable_pruning(true);

                (Arc::new(format), path, DEFAULT_PARQUET_EXTENSION)
            }
            other => {
                unimplemented!("Invalid file format '{}'", other);
            }
        };
    let schema = Arc::new(get_schema(table));

    let options = ListingOptions {
        format,
        file_extension: extension.to_owned(),
        target_partitions,
        collect_stat: ctx.config.collect_statistics,
        table_partition_cols: vec![],
    };

    let table_path = ListingTableUrl::parse(path)?;
    let config = ListingTableConfig::new(table_path).with_listing_options(options);

    let config = if table_format == "parquet" {
        config.infer_schema(ctx).await?
    } else {
        config.with_schema(schema)
    };

    Ok(Arc::new(ListingTable::try_new(config)?))
}

fn get_schema(table: &str) -> Schema {
    // note that the schema intentionally uses signed integers so that any generated Parquet
    // files can also be used to benchmark tools that only support signed integers, such as
    // Apache Spark

    match table {
        "part" => Schema::new(vec![
            Field::new("p_partkey", DataType::Int64, false),
            Field::new("p_name", DataType::Utf8, false),
            Field::new("p_mfgr", DataType::Utf8, false),
            Field::new("p_brand", DataType::Utf8, false),
            Field::new("p_type", DataType::Utf8, false),
            Field::new("p_size", DataType::Int32, false),
            Field::new("p_container", DataType::Utf8, false),
            Field::new("p_retailprice", DataType::Decimal128(15, 2), false),
            Field::new("p_comment", DataType::Utf8, false),
        ]),

        "supplier" => Schema::new(vec![
            Field::new("s_suppkey", DataType::Int64, false),
            Field::new("s_name", DataType::Utf8, false),
            Field::new("s_address", DataType::Utf8, false),
            Field::new("s_nationkey", DataType::Int64, false),
            Field::new("s_phone", DataType::Utf8, false),
            Field::new("s_acctbal", DataType::Decimal128(15, 2), false),
            Field::new("s_comment", DataType::Utf8, false),
        ]),

        "partsupp" => Schema::new(vec![
            Field::new("ps_partkey", DataType::Int64, false),
            Field::new("ps_suppkey", DataType::Int64, false),
            Field::new("ps_availqty", DataType::Int32, false),
            Field::new("ps_supplycost", DataType::Decimal128(15, 2), false),
            Field::new("ps_comment", DataType::Utf8, false),
        ]),

        "customer" => Schema::new(vec![
            Field::new("c_custkey", DataType::Int64, false),
            Field::new("c_name", DataType::Utf8, false),
            Field::new("c_address", DataType::Utf8, false),
            Field::new("c_nationkey", DataType::Int64, false),
            Field::new("c_phone", DataType::Utf8, false),
            Field::new("c_acctbal", DataType::Decimal128(15, 2), false),
            Field::new("c_mktsegment", DataType::Utf8, false),
            Field::new("c_comment", DataType::Utf8, false),
        ]),

        "orders" => Schema::new(vec![
            Field::new("o_orderkey", DataType::Int64, false),
            Field::new("o_custkey", DataType::Int64, false),
            Field::new("o_orderstatus", DataType::Utf8, false),
            Field::new("o_totalprice", DataType::Decimal128(15, 2), false),
            Field::new("o_orderdate", DataType::Date32, false),
            Field::new("o_orderpriority", DataType::Utf8, false),
            Field::new("o_clerk", DataType::Utf8, false),
            Field::new("o_shippriority", DataType::Int32, false),
            Field::new("o_comment", DataType::Utf8, false),
        ]),

        "lineitem" => Schema::new(vec![
            Field::new("l_orderkey", DataType::Int64, false),
            Field::new("l_partkey", DataType::Int64, false),
            Field::new("l_suppkey", DataType::Int64, false),
            Field::new("l_linenumber", DataType::Int32, false),
            Field::new("l_quantity", DataType::Decimal128(15, 2), false),
            Field::new("l_extendedprice", DataType::Decimal128(15, 2), false),
            Field::new("l_discount", DataType::Decimal128(15, 2), false),
            Field::new("l_tax", DataType::Decimal128(15, 2), false),
            Field::new("l_returnflag", DataType::Utf8, false),
            Field::new("l_linestatus", DataType::Utf8, false),
            Field::new("l_shipdate", DataType::Date32, false),
            Field::new("l_commitdate", DataType::Date32, false),
            Field::new("l_receiptdate", DataType::Date32, false),
            Field::new("l_shipinstruct", DataType::Utf8, false),
            Field::new("l_shipmode", DataType::Utf8, false),
            Field::new("l_comment", DataType::Utf8, false),
        ]),

        "nation" => Schema::new(vec![
            Field::new("n_nationkey", DataType::Int64, false),
            Field::new("n_name", DataType::Utf8, false),
            Field::new("n_regionkey", DataType::Int64, false),
            Field::new("n_comment", DataType::Utf8, false),
        ]),

        "region" => Schema::new(vec![
            Field::new("r_regionkey", DataType::Int64, false),
            Field::new("r_name", DataType::Utf8, false),
            Field::new("r_comment", DataType::Utf8, false),
        ]),

        _ => unimplemented!(),
    }
}

#[derive(Debug, Serialize)]
struct BenchmarkRun {
    /// Benchmark crate version
    benchmark_version: String,
    /// DataFusion crate version
    datafusion_version: String,
    /// Number of CPU cores
    num_cpus: usize,
    /// Start time
    start_time: u64,
    /// CLI arguments
    arguments: Vec<String>,
    /// query number
    query: usize,
    /// list of individual run times and row counts
    iterations: Vec<QueryResult>,
}

impl BenchmarkRun {
    fn new(query: usize) -> Self {
        Self {
            benchmark_version: env!("CARGO_PKG_VERSION").to_owned(),
            datafusion_version: DATAFUSION_VERSION.to_owned(),
            num_cpus: num_cpus::get(),
            start_time: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("current time is later than UNIX_EPOCH")
                .as_secs(),
            arguments: std::env::args()
                .skip(1)
                .into_iter()
                .collect::<Vec<String>>(),
            query,
            iterations: vec![],
        }
    }

    fn add_result(&mut self, elapsed: f64, row_count: usize) {
        self.iterations.push(QueryResult { elapsed, row_count })
    }
}

#[derive(Debug, Serialize)]
struct QueryResult {
    elapsed: f64,
    row_count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::io::{BufRead, BufReader};
    use std::ops::{Div, Mul};
    use std::sync::Arc;

    use datafusion::arrow::array::*;
    use datafusion::arrow::util::display::array_value_to_string;
    use datafusion::logical_expr::expr::Cast;
    use datafusion::logical_expr::Expr;
    use datafusion::logical_expr::Expr::ScalarFunction;
    use datafusion::sql::TableReference;

    const QUERY_LIMIT: [Option<usize>; 22] = [
        None,
        Some(100),
        Some(10),
        None,
        None,
        None,
        None,
        None,
        None,
        Some(20),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(100),
        None,
        None,
        Some(100),
        None,
    ];

    #[tokio::test]
    async fn q1_expected_plan() -> Result<()> {
        expected_plan(1).await
    }

    #[tokio::test]
    async fn q2_expected_plan() -> Result<()> {
        expected_plan(2).await
    }

    #[tokio::test]
    async fn q3_expected_plan() -> Result<()> {
        expected_plan(3).await
    }

    #[tokio::test]
    async fn q4_expected_plan() -> Result<()> {
        expected_plan(4).await
    }

    #[tokio::test]
    async fn q5_expected_plan() -> Result<()> {
        expected_plan(5).await
    }

    #[tokio::test]
    async fn q6_expected_plan() -> Result<()> {
        expected_plan(6).await
    }

    #[tokio::test]
    async fn q7_expected_plan() -> Result<()> {
        expected_plan(7).await
    }

    #[tokio::test]
    async fn q8_expected_plan() -> Result<()> {
        expected_plan(8).await
    }

    #[tokio::test]
    async fn q9_expected_plan() -> Result<()> {
        expected_plan(9).await
    }

    #[tokio::test]
    async fn q10_expected_plan() -> Result<()> {
        expected_plan(10).await
    }

    #[tokio::test]
    async fn q11_expected_plan() -> Result<()> {
        expected_plan(11).await
    }

    #[tokio::test]
    async fn q12_expected_plan() -> Result<()> {
        expected_plan(12).await
    }

    #[tokio::test]
    async fn q13_expected_plan() -> Result<()> {
        expected_plan(13).await
    }

    #[tokio::test]
    async fn q14_expected_plan() -> Result<()> {
        expected_plan(14).await
    }

    #[tokio::test]
    async fn q15_expected_plan() -> Result<()> {
        expected_plan(15).await
    }

    #[tokio::test]
    async fn q16_expected_plan() -> Result<()> {
        expected_plan(16).await
    }

    /// This query produces different plans depending on operating system. The difference is
    /// due to re-writing the following expression:
    ///
    /// `sum(l_extendedprice) / 7.0 as avg_yearly`
    ///
    /// Linux:   Decimal128(Some(7000000000000000195487369212723200),38,33)
    /// Windows: Decimal128(Some(6999999999999999042565864605876224),38,33)
    ///
    /// See https://github.com/apache/arrow-datafusion/issues/3791
    #[tokio::test]
    #[ignore]
    async fn q17_expected_plan() -> Result<()> {
        expected_plan(17).await
    }

    #[tokio::test]
    async fn q18_expected_plan() -> Result<()> {
        expected_plan(18).await
    }

    #[tokio::test]
    async fn q19_expected_plan() -> Result<()> {
        expected_plan(19).await
    }

    #[tokio::test]
    async fn q20_expected_plan() -> Result<()> {
        expected_plan(20).await
    }

    #[tokio::test]
    async fn q21_expected_plan() -> Result<()> {
        expected_plan(21).await
    }

    #[tokio::test]
    async fn q22_expected_plan() -> Result<()> {
        expected_plan(22).await
    }

    async fn expected_plan(query: usize) -> Result<()> {
        let ctx = SessionContext::new();
        for table in TABLES {
            let table = table.to_string();
            let schema = get_schema(&table);
            let mem_table = MemTable::try_new(Arc::new(schema), vec![])?;
            ctx.register_table(
                TableReference::from(table.as_str()),
                Arc::new(mem_table),
            )?;
        }

        let mut actual = String::new();
        let sql = get_query_sql(query)?;
        for sql in &sql {
            let df = ctx.sql(sql.as_str()).await?;
            let plan = df.to_logical_plan()?;
            if !actual.is_empty() {
                actual += "\n";
            }
            use std::fmt::Write as _;
            write!(actual, "{}", plan.display_indent()).unwrap();
        }

        let possibilities = vec![
            format!("expected-plans/q{}.txt", query),
            format!("benchmarks/expected-plans/q{}.txt", query),
        ];

        let mut found = false;
        for path in &possibilities {
            let path = Path::new(&path);
            if let Ok(expected) = read_text_file(path) {
                assert_eq!(expected, actual,
                    // generate output that is easier to copy/paste/update
                    "\n\nMismatch of expected content in: {:?}\nExpected:\n\n{}\n\nActual:\n\n{}\n\n",
                    path, expected, actual);
                found = true;
                break;
            }
        }
        assert!(found);

        Ok(())
    }

    /// we need to read line by line and add \n so tests work on Windows
    fn read_text_file(path: &Path) -> Result<String> {
        let file = File::open(path)?;
        let reader = BufReader::new(file);
        let mut str = String::new();
        for line in reader.lines() {
            let line = line?;
            if !str.is_empty() {
                str += "\n";
            }
            str += &line;
        }
        Ok(str)
    }

    #[tokio::test]
    async fn q1() -> Result<()> {
        verify_query(1).await
    }

    #[tokio::test]
    async fn q2() -> Result<()> {
        verify_query(2).await
    }

    #[tokio::test]
    async fn q3() -> Result<()> {
        verify_query(3).await
    }

    #[tokio::test]
    async fn q4() -> Result<()> {
        verify_query(4).await
    }

    #[tokio::test]
    async fn q5() -> Result<()> {
        verify_query(5).await
    }

    #[tokio::test]
    async fn q6() -> Result<()> {
        verify_query(6).await
    }

    #[tokio::test]
    async fn q7() -> Result<()> {
        verify_query(7).await
    }

    #[tokio::test]
    async fn q8() -> Result<()> {
        verify_query(8).await
    }

    #[tokio::test]
    async fn q9() -> Result<()> {
        verify_query(9).await
    }

    #[tokio::test]
    async fn q10() -> Result<()> {
        verify_query(10).await
    }

    #[tokio::test]
    async fn q11() -> Result<()> {
        verify_query(11).await
    }

    #[tokio::test]
    async fn q12() -> Result<()> {
        verify_query(12).await
    }

    #[tokio::test]
    async fn q13() -> Result<()> {
        verify_query(13).await
    }

    #[tokio::test]
    async fn q14() -> Result<()> {
        verify_query(14).await
    }

    #[tokio::test]
    async fn q15() -> Result<()> {
        verify_query(15).await
    }

    #[tokio::test]
    async fn q16() -> Result<()> {
        verify_query(16).await
    }

    #[tokio::test]
    async fn q17() -> Result<()> {
        verify_query(17).await
    }

    #[tokio::test]
    async fn q18() -> Result<()> {
        verify_query(18).await
    }

    #[tokio::test]
    async fn q19() -> Result<()> {
        verify_query(19).await
    }

    #[tokio::test]
    async fn q20() -> Result<()> {
        verify_query(20).await
    }

    #[tokio::test]
    async fn q21() -> Result<()> {
        verify_query(21).await
    }

    #[tokio::test]
    async fn q22() -> Result<()> {
        verify_query(22).await
    }

    #[tokio::test]
    async fn run_q1() -> Result<()> {
        run_query(1).await
    }

    #[tokio::test]
    async fn run_q2() -> Result<()> {
        run_query(2).await
    }

    #[tokio::test]
    async fn run_q3() -> Result<()> {
        run_query(3).await
    }

    #[tokio::test]
    async fn run_q4() -> Result<()> {
        run_query(4).await
    }

    #[tokio::test]
    async fn run_q5() -> Result<()> {
        run_query(5).await
    }

    #[tokio::test]
    async fn run_q6() -> Result<()> {
        run_query(6).await
    }

    #[tokio::test]
    async fn run_q7() -> Result<()> {
        run_query(7).await
    }

    #[tokio::test]
    async fn run_q8() -> Result<()> {
        run_query(8).await
    }

    #[tokio::test]
    async fn run_q9() -> Result<()> {
        run_query(9).await
    }

    #[tokio::test]
    async fn run_q10() -> Result<()> {
        run_query(10).await
    }

    #[tokio::test]
    async fn run_q11() -> Result<()> {
        run_query(11).await
    }

    #[tokio::test]
    async fn run_q12() -> Result<()> {
        run_query(12).await
    }

    #[tokio::test]
    async fn run_q13() -> Result<()> {
        run_query(13).await
    }

    #[tokio::test]
    async fn run_q14() -> Result<()> {
        run_query(14).await
    }

    #[tokio::test]
    async fn run_q15() -> Result<()> {
        run_query(15).await
    }

    #[tokio::test]
    async fn run_q16() -> Result<()> {
        run_query(16).await
    }

    #[tokio::test]
    async fn run_q17() -> Result<()> {
        run_query(17).await
    }

    #[tokio::test]
    async fn run_q18() -> Result<()> {
        run_query(18).await
    }

    #[tokio::test]
    async fn run_q19() -> Result<()> {
        run_query(19).await
    }

    #[tokio::test]
    async fn run_q20() -> Result<()> {
        run_query(20).await
    }

    #[tokio::test]
    async fn run_q21() -> Result<()> {
        run_query(21).await
    }

    #[tokio::test]
    async fn run_q22() -> Result<()> {
        run_query(22).await
    }

    /// Specialised String representation
    fn col_str(column: &ArrayRef, row_index: usize) -> String {
        if column.is_null(row_index) {
            return "NULL".to_string();
        }

        array_value_to_string(column, row_index).unwrap()
    }

    /// Converts the results into a 2d array of strings, `result[row][column]`
    /// Special cases nulls to NULL for testing
    fn result_vec(results: &[RecordBatch]) -> Vec<Vec<String>> {
        let mut result = vec![];
        for batch in results {
            for row_index in 0..batch.num_rows() {
                let row_vec = batch
                    .columns()
                    .iter()
                    .map(|column| col_str(column, row_index))
                    .collect();
                result.push(row_vec);
            }
        }
        result
    }

    fn get_answer_schema(n: usize) -> Schema {
        match n {
            1 => Schema::new(vec![
                Field::new("l_returnflag", DataType::Utf8, true),
                Field::new("l_linestatus", DataType::Utf8, true),
                Field::new("sum_qty", DataType::Decimal128(15, 2), true),
                Field::new("sum_base_price", DataType::Decimal128(15, 2), true),
                Field::new("sum_disc_price", DataType::Decimal128(15, 2), true),
                Field::new("sum_charge", DataType::Decimal128(15, 2), true),
                Field::new("avg_qty", DataType::Decimal128(15, 2), true),
                Field::new("avg_price", DataType::Decimal128(15, 2), true),
                Field::new("avg_disc", DataType::Decimal128(15, 2), true),
                Field::new("count_order", DataType::Int64, true),
            ]),

            2 => Schema::new(vec![
                Field::new("s_acctbal", DataType::Decimal128(15, 2), true),
                Field::new("s_name", DataType::Utf8, true),
                Field::new("n_name", DataType::Utf8, true),
                Field::new("p_partkey", DataType::Int64, true),
                Field::new("p_mfgr", DataType::Utf8, true),
                Field::new("s_address", DataType::Utf8, true),
                Field::new("s_phone", DataType::Utf8, true),
                Field::new("s_comment", DataType::Utf8, true),
            ]),

            3 => Schema::new(vec![
                Field::new("l_orderkey", DataType::Int64, true),
                Field::new("revenue", DataType::Decimal128(15, 2), true),
                Field::new("o_orderdate", DataType::Date32, true),
                Field::new("o_shippriority", DataType::Int32, true),
            ]),

            4 => Schema::new(vec![
                Field::new("o_orderpriority", DataType::Utf8, true),
                Field::new("order_count", DataType::Int64, true),
            ]),

            5 => Schema::new(vec![
                Field::new("n_name", DataType::Utf8, true),
                Field::new("revenue", DataType::Decimal128(15, 2), true),
            ]),

            6 => Schema::new(vec![Field::new(
                "revenue",
                DataType::Decimal128(15, 2),
                true,
            )]),

            7 => Schema::new(vec![
                Field::new("supp_nation", DataType::Utf8, true),
                Field::new("cust_nation", DataType::Utf8, true),
                Field::new("l_year", DataType::Int32, true),
                Field::new("revenue", DataType::Decimal128(15, 2), true),
            ]),

            8 => Schema::new(vec![
                Field::new("o_year", DataType::Int32, true),
                Field::new("mkt_share", DataType::Decimal128(15, 2), true),
            ]),

            9 => Schema::new(vec![
                Field::new("nation", DataType::Utf8, true),
                Field::new("o_year", DataType::Int32, true),
                Field::new("sum_profit", DataType::Decimal128(15, 2), true),
            ]),

            10 => Schema::new(vec![
                Field::new("c_custkey", DataType::Int64, true),
                Field::new("c_name", DataType::Utf8, true),
                Field::new("revenue", DataType::Decimal128(15, 2), true),
                Field::new("c_acctbal", DataType::Decimal128(15, 2), true),
                Field::new("n_name", DataType::Utf8, true),
                Field::new("c_address", DataType::Utf8, true),
                Field::new("c_phone", DataType::Utf8, true),
                Field::new("c_comment", DataType::Utf8, true),
            ]),

            11 => Schema::new(vec![
                Field::new("ps_partkey", DataType::Int64, true),
                Field::new("value", DataType::Decimal128(15, 2), true),
            ]),

            12 => Schema::new(vec![
                Field::new("l_shipmode", DataType::Utf8, true),
                Field::new("high_line_count", DataType::Int64, true),
                Field::new("low_line_count", DataType::Int64, true),
            ]),

            13 => Schema::new(vec![
                Field::new("c_count", DataType::Int64, true),
                Field::new("custdist", DataType::Int64, true),
            ]),

            14 => Schema::new(vec![Field::new("promo_revenue", DataType::Float64, true)]),

            15 => Schema::new(vec![
                Field::new("s_suppkey", DataType::Int64, true),
                Field::new("s_name", DataType::Utf8, true),
                Field::new("s_address", DataType::Utf8, true),
                Field::new("s_phone", DataType::Utf8, true),
                Field::new("total_revenue", DataType::Decimal128(15, 2), true),
            ]),

            16 => Schema::new(vec![
                Field::new("p_brand", DataType::Utf8, true),
                Field::new("p_type", DataType::Utf8, true),
                Field::new("p_size", DataType::Int32, true),
                Field::new("supplier_cnt", DataType::Int64, true),
            ]),

            17 => Schema::new(vec![Field::new("avg_yearly", DataType::Float64, true)]),

            18 => Schema::new(vec![
                Field::new("c_name", DataType::Utf8, true),
                Field::new("c_custkey", DataType::Int64, true),
                Field::new("o_orderkey", DataType::Int64, true),
                Field::new("o_orderdate", DataType::Date32, true),
                Field::new("o_totalprice", DataType::Decimal128(15, 2), true),
                Field::new("sum_l_quantity", DataType::Decimal128(15, 2), true),
            ]),

            19 => Schema::new(vec![Field::new(
                "revenue",
                DataType::Decimal128(15, 2),
                true,
            )]),

            20 => Schema::new(vec![
                Field::new("s_name", DataType::Utf8, true),
                Field::new("s_address", DataType::Utf8, true),
            ]),

            21 => Schema::new(vec![
                Field::new("s_name", DataType::Utf8, true),
                Field::new("numwait", DataType::Int64, true),
            ]),

            22 => Schema::new(vec![
                Field::new("cntrycode", DataType::Utf8, true),
                Field::new("numcust", DataType::Int64, true),
                Field::new("totacctbal", DataType::Decimal128(15, 2), true),
            ]),

            _ => unimplemented!(),
        }
    }

    // convert expected schema to all utf8 so columns can be read as strings to be parsed separately
    // this is due to the fact that the csv parser cannot handle leading/trailing spaces
    fn string_schema(schema: Schema) -> Schema {
        Schema::new(
            schema
                .fields()
                .iter()
                .map(|field| {
                    Field::new(
                        Field::name(field),
                        DataType::Utf8,
                        Field::is_nullable(field),
                    )
                })
                .collect::<Vec<Field>>(),
        )
    }

    async fn transform_actual_result(
        result: Vec<RecordBatch>,
        n: usize,
    ) -> Result<Vec<RecordBatch>> {
        // to compare the recorded answers to the answers we got back from running the query,
        // we need to round the decimal columns and trim the Utf8 columns
        let ctx = SessionContext::new();
        let result_schema = result[0].schema();
        let table = Arc::new(MemTable::try_new(result_schema.clone(), vec![result])?);
        let mut df = ctx.read_table(table)?
            .select(
                result_schema
                    .fields
                    .iter()
                    .map(|field| {
                        match Field::data_type(field) {
                            DataType::Decimal128(_, _) => {
                                // if decimal, then round it to 2 decimal places like the answers
                                // round() doesn't support the second argument for decimal places to round to
                                // this can be simplified to remove the mul and div when
                                // https://github.com/apache/arrow-datafusion/issues/2420 is completed
                                // cast it back to an over-sized Decimal with 2 precision when done rounding
                                let round = Box::new(ScalarFunction {
                                    fun: datafusion::logical_expr::BuiltinScalarFunction::Round,
                                    args: vec![col(Field::name(field)).mul(lit(100))],
                                }.div(lit(100)));
                                Expr::Alias(
                                    Box::new(Expr::Cast(Cast::new(
                                        round,
                                        DataType::Decimal128(38, 2),
                                    ))),
                                    Field::name(field).to_string(),
                                )
                            }
                            DataType::Utf8 => {
                                // if string, then trim it like the answers got trimmed
                                Expr::Alias(
                                    Box::new(trim(col(Field::name(field)))),
                                    Field::name(field).to_string(),
                                )
                            }
                            _ => {
                                col(Field::name(field))
                            }
                        }
                    }).collect()
            )?;
        if let Some(x) = QUERY_LIMIT[n - 1] {
            df = df.limit(0, Some(x))?;
        }

        let df = df.collect().await?;
        Ok(df)
    }

    async fn run_query(n: usize) -> Result<()> {
        // Tests running query with empty tables, to see whether they run successfully.

        let config = SessionConfig::new()
            .with_target_partitions(1)
            .with_batch_size(10);
        let ctx = SessionContext::with_config(config);

        for &table in TABLES {
            let schema = get_schema(table);
            let batch = RecordBatch::new_empty(Arc::new(schema.to_owned()));

            ctx.register_batch(table, batch)?;
        }

        let sql = &get_query_sql(n)?;
        for query in sql {
            execute_query(&ctx, query, false).await?;
        }

        Ok(())
    }

    /// compares query results against stored answers from the git repo
    /// verifies that:
    ///  * datatypes returned in columns is correct
    ///  * the correct number of rows are returned
    ///  * the content of the rows is correct
    async fn verify_query(n: usize) -> Result<()> {
        if let Ok(path) = env::var("TPCH_DATA") {
            // load expected answers from tpch-dbgen
            // read csv as all strings, trim and cast to expected type as the csv string
            // to value parser does not handle data with leading/trailing spaces
            let ctx = SessionContext::new();
            let schema = string_schema(get_answer_schema(n));
            let options = CsvReadOptions::new()
                .schema(&schema)
                .delimiter(b'|')
                .file_extension(".out");
            let df = ctx
                .read_csv(&format!("{}/answers/q{}.out", path, n), options)
                .await?;
            let df = df.select(
                get_answer_schema(n)
                    .fields()
                    .iter()
                    .map(|field| {
                        match Field::data_type(field) {
                            DataType::Decimal128(_, _) => {
                                // there's no support for casting from Utf8 to Decimal, so
                                // we'll cast from Utf8 to Float64 to Decimal for Decimal types
                                let inner_cast = Box::new(Expr::Cast(Cast::new(
                                    Box::new(trim(col(Field::name(field)))),
                                    DataType::Float64,
                                )));
                                Expr::Alias(
                                    Box::new(Expr::Cast(Cast::new(
                                        inner_cast,
                                        Field::data_type(field).to_owned(),
                                    ))),
                                    Field::name(field).to_string(),
                                )
                            }
                            _ => Expr::Alias(
                                Box::new(Expr::Cast(Cast::new(
                                    Box::new(trim(col(Field::name(field)))),
                                    Field::data_type(field).to_owned(),
                                ))),
                                Field::name(field).to_string(),
                            ),
                        }
                    })
                    .collect::<Vec<Expr>>(),
            )?;
            let expected = df.collect().await?;

            // run the query to compute actual results of the query
            let opt = DataFusionBenchmarkOpt {
                query: n,
                debug: false,
                iterations: 1,
                partitions: 2,
                batch_size: 8192,
                path: PathBuf::from(path.to_string()),
                file_format: "tbl".to_string(),
                mem_table: false,
                output_path: None,
                disable_statistics: false,
            };
            let actual = benchmark_datafusion(opt).await?;

            let transformed = transform_actual_result(actual, n).await?;

            // assert schema data types match
            let transformed_fields = &transformed[0].schema().fields;
            let expected_fields = &expected[0].schema().fields;
            let schema_matches = transformed_fields
                .iter()
                .zip(expected_fields.iter())
                .all(|(t, e)| match t.data_type() {
                    DataType::Decimal128(_, _) => {
                        matches!(e.data_type(), DataType::Decimal128(_, _))
                    }
                    data_type => data_type == e.data_type(),
                });
            assert!(schema_matches);

            // convert both datasets to Vec<Vec<String>> for simple comparison
            let expected_vec = result_vec(&expected);
            let actual_vec = result_vec(&transformed);

            // basic result comparison
            assert_eq!(expected_vec.len(), actual_vec.len());

            // compare each row. this works as all TPC-H queries have deterministically ordered results
            for i in 0..actual_vec.len() {
                assert_eq!(expected_vec[i], actual_vec[i]);
            }
        } else {
            println!("TPCH_DATA environment variable not set, skipping test");
        }

        Ok(())
    }
}
