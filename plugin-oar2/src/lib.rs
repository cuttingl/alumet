use alumet::{
    measurement::{MeasurementAccumulator, MeasurementPoint, Timestamp},
    metrics::TypedMetricId,
    pipeline::{runtime::ControlHandle, trigger::TriggerSpec, PollError, Source},
    plugin::{
        rust::{deserialize_config, serialize_config, AlumetPlugin},
        AlumetStart, ConfigTable, Plugin,
    },
    resources::{Resource, ResourceConsumer},
    units::{PrefixedUnit, Unit},
};
use anyhow::Context;
use notify::{Event, EventHandler, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    io::{Read, Seek},
    path::PathBuf,
    time::Duration,
};

#[derive(Debug)]
pub struct Oar2Plugin {
    config: Config,
    metrics: Option<Metrics>,
    watcher: Option<RecommendedWatcher>,
}
#[derive(Debug, Clone)]
pub struct Metrics {
    cpu_metric: TypedMetricId<u64>,
    memory_metric: TypedMetricId<u64>,
}

#[derive(Serialize, Deserialize, Debug)]
struct Config {
    path: PathBuf,
    #[serde(with = "humantime_serde")]
    poll_interval: Duration,
}

#[derive(Debug)]
struct OarJobSource {
    cpu_metric: TypedMetricId<u64>,
    memory_metric: TypedMetricId<u64>,
    cgroup_cpu_file: File,
    cgroup_memory_file: File,
    cpu_file_path: PathBuf,
    memory_file_path: PathBuf,
    job_id: u64,
}

impl AlumetPlugin for Oar2Plugin {
    fn name() -> &'static str {
        "oar2-plugin"
    }

    fn version() -> &'static str {
        "0.1.0"
    }

    fn default_config() -> anyhow::Result<Option<alumet::plugin::ConfigTable>> {
        Ok(Some(serialize_config(Config::default())?))
    }

    fn init(config: ConfigTable) -> anyhow::Result<Box<Self>> {
        let config: Config = deserialize_config(config)?;
        Ok(Box::new(Oar2Plugin {
            config,
            metrics: None,
            watcher: None,
        }))
    }

    fn start(&mut self, alumet: &mut AlumetStart) -> Result<(), anyhow::Error> {
        let cpu_metric = alumet.create_metric::<u64>(
            "cpu_time",
            PrefixedUnit::nano(Unit::Second),
            "Total CPU time consumed by the cgroup (in nanoseconds).",
        )?;
        let memory_metric = alumet.create_metric::<u64>(
            "memory_usage",
            Unit::Unity,
            "Total memory usage by the cgroup (in bytes).",
        )?;

        self.metrics = Some(Metrics {
            cpu_metric,
            memory_metric,
        });

        let cgroup_cpu_path = self.config.path.join("cpuacct/oar");
        let cgroup_memory_path = self.config.path.join("memory/oar");

        // Scanning to check if there are jobs already running
        for entry in fs::read_dir(&cgroup_cpu_path)
            .with_context(|| format!("Invalid oar cpuacct cgroup path, {cgroup_cpu_path:?}"))?
        {
            let entry = entry?;

            let job_name = entry.file_name();
            let job_name = job_name.into_string().unwrap();

            if entry.file_type()?.is_dir() && job_name.chars().any(|c| c.is_numeric()) {
                let job_separated = job_name.split_once("_");
                let job_id = job_separated.context("Invalid oar cgroup.")?.1.parse()?;

                let cpu_job_path = cgroup_cpu_path.join(&job_name);
                let memory_job_path = cgroup_memory_path.join(&job_name);

                let cpu_file_path = cpu_job_path.join("cpuacct.usage");
                let memory_file_path = memory_job_path.join("memory.usage_in_bytes");

                let cgroup_cpu_file = File::open(&cpu_file_path)
                    .with_context(|| format!("Failed to open CPU usage file at {}", cpu_file_path.display()))?;
                let cgroup_memory_file = File::open(&memory_file_path)
                    .with_context(|| format!("Failed to open memory usage file at {}", memory_file_path.display()))?;

                let initial_source = Box::new(OarJobSource {
                    cpu_metric,
                    memory_metric,
                    cgroup_cpu_file,
                    cgroup_memory_file,
                    cpu_file_path,
                    memory_file_path,
                    job_id,
                });

                alumet.add_source(initial_source, TriggerSpec::at_interval(self.config.poll_interval));
            }
        }
        Ok(())
    }

    fn post_pipeline_start(&mut self, pipeline: &mut alumet::pipeline::runtime::RunningPipeline) -> anyhow::Result<()> {
        let control_handle = pipeline.control_handle();
        let config_path = self.config.path.clone();
        let plugin_name = self.name().to_owned();

        let metrics = self.metrics.clone().unwrap();
        let cpu_metric = metrics.cpu_metric;
        let memory_metric = metrics.memory_metric;
        let poll_interval = self.config.poll_interval;

        struct JobDetector {
            config_path: PathBuf,
            cpu_metric: TypedMetricId<u64>,
            memory_metric: TypedMetricId<u64>,
            control_handle: ControlHandle,
            plugin_name: String,
            poll_interval: Duration,
        }

        impl EventHandler for JobDetector {
            fn handle_event(&mut self, event: Result<Event, notify::Error>) {
                log::debug!("Handle event function");
                if let Ok(Event {
                    kind: EventKind::Create(_),
                    paths,
                    ..
                }) = event
                {
                    log::debug!("Paths: {paths:?}");
                    for path in paths {
                        if let Some(job_name) = path.file_name() {
                            let job_name = job_name.to_str().unwrap();
                            if job_name.chars().any(|c| c.is_numeric()) {
                                let job_separated = job_name.split_once("_");
                                let job_id = job_separated.context("Invalid oar cgroup").unwrap().1.parse().unwrap();

                                let cpu_path = self.config_path.join("cpuacct/oar").join(&job_name);
                                log::debug!("CPU path {cpu_path:?}");
                                let memory_path = self.config_path.join("memory/oar").join(&job_name);
                                log::debug!("Memory path {memory_path:?}");

                                let cpu_file_path = cpu_path.join("cpuacct.usage");
                                log::debug!("CPU file path {cpu_file_path:?}");
                                let memory_file_path = memory_path.join("memory.usage_in_bytes");
                                log::debug!("Memory file path {memory_file_path:?}");

                                if let (Ok(cgroup_cpu_file), Ok(cgroup_memory_file)) =
                                    (File::open(&cpu_file_path), File::open(&memory_file_path))
                                {
                                    let new_source = Box::new(OarJobSource {
                                        cpu_metric: self.cpu_metric,
                                        memory_metric: self.memory_metric,
                                        cgroup_cpu_file,
                                        cgroup_memory_file,
                                        cpu_file_path,
                                        memory_file_path,
                                        job_id,
                                    });

                                    let source_name = job_name.to_string();

                                    self.control_handle.add_source(
                                        self.plugin_name.clone(),
                                        source_name,
                                        new_source,
                                        TriggerSpec::at_interval(self.poll_interval),
                                    );
                                }
                            }
                        }
                    }
                } else if let Err(e) = event {
                    eprintln!("watch error: {:?}", e);
                }
            }
        }

        let handler = JobDetector {
            config_path: config_path.clone(),
            cpu_metric,
            memory_metric,
            control_handle,
            plugin_name,
            poll_interval,
        };
        let mut watcher = notify::recommended_watcher(handler)?;

        watcher.watch(&config_path.join("cpuacct/oar"), RecursiveMode::NonRecursive)?;
        watcher.watch(&config_path.join("memory/oar"), RecursiveMode::NonRecursive)?;

        self.watcher = Some(watcher);

        Ok(())
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

impl Default for Config {
    fn default() -> Self {
        let mut path = PathBuf::new();
        path.push("/sys/fs/cgroup");
        Self {
            path,
            poll_interval: Duration::from_secs(1),
        }
    }
}

impl Source for OarJobSource {
    fn poll(&mut self, measurements: &mut MeasurementAccumulator, timestamp: Timestamp) -> Result<(), PollError> {
        // log::debug!("OarJobSource::poll at {timestamp:?}");
        let cpu_usage_file = &mut self.cgroup_cpu_file;
        cpu_usage_file.rewind()?;
        let mut cpu_usage = String::new();
        cpu_usage_file.read_to_string(&mut cpu_usage)?;
        let memory_usage_file = &mut self.cgroup_memory_file;
        memory_usage_file.rewind()?;
        let mut memory_usage = String::new();
        memory_usage_file.read_to_string(&mut memory_usage)?;
        let cpu_usage_u64 = cpu_usage.trim().parse::<u64>()?;
        let memory_usage_u64 = memory_usage.trim().parse::<u64>()?;

        measurements.push(
            MeasurementPoint::new(
                timestamp,
                self.cpu_metric,
                Resource::LocalMachine,
                ResourceConsumer::ControlGroup {
                    path: (self.cpu_file_path.to_str().unwrap().to_owned().into()),
                },
                cpu_usage_u64,
            )
            .with_attr("oar_job_id", self.job_id),
        );

        measurements.push(
            MeasurementPoint::new(
                timestamp,
                self.memory_metric,
                Resource::LocalMachine,
                ResourceConsumer::ControlGroup {
                    path: (self.memory_file_path.to_str().unwrap().to_owned().into()),
                },
                memory_usage_u64,
            )
            .with_attr("oar_job_id", self.job_id),
        );

        Ok(())
    }
}
