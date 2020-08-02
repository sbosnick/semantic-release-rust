// Copyright 2020 Steven Bosnick
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE-2.0 or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Implementation of the sementic release steps to for integraing a cargo-based Rust
//! project.

#![forbid(unsafe_code)]
#![deny(warnings, missing_docs)]

use std::env;
use std::fmt;
use std::fs;
use std::io::Write;
use std::{
    path::{Path, PathBuf},
    result,
};

use guppy::{
    graph::{DependencyDirection, PackageGraph, PackageLink, PackageSource},
    MetadataCommand,
};
use itertools::Itertools;
use log::{debug, error, info, trace};
use toml_edit::{Document, InlineTable, Item, Table, Value};

mod error;

pub use error::{CargoTomlError, Error, Result};

/// Verify that the conditions for a release are satisfied.
///
/// The conditions for a release checked by this function are:
///
///    1. That the CARGO_REGISTRY_TOKEN environment variable is set and is
///       non-empty.
///    2. That it can construct the graph of all of the dependencies in the
///       workspace.
///    3. That the dependencies and build-dependencies of all of crates in the
///       workspace are suitable for publishing to `crates.io`.
///
/// If `manifest_path` is provided then it is expect to give the path to the
/// `Cargo.toml` file for the root of the workspace. If `manifest_path` is `None`
/// then `verify_conditions` will look for the root of the workspace in a
/// `Cargo.toml` file in the current directory. If one of the conditions for a
/// release are not satisfied then an explination for that will be written to
/// `output`.
///
/// This implments the `verifyConditions` step for `sementic-release` for a
/// Cargo-based rust workspace.
pub fn verify_conditions(
    mut output: impl Write,
    manifest_path: Option<impl AsRef<Path>>,
) -> Result<()> {
    info!("Checking CARGO_REGISTRY_TOKEN");
    env::var_os("CARGO_REGISTRY_TOKEN")
        .and_then(|val| if val.is_empty() { None } else { Some(()) })
        .ok_or_else(|| {
            writeln!(output, "CARGO_REGISTRY_TOKEN empty or not set.")
                .map_err(Error::output_error)
                .and_then::<(), _>(|()| {
                    Err(Error::verify_error("CARGO_REGISTRY_TOKEN empty or not set"))
                })
                .unwrap_err()
        })?;

    info!("Checking that workspace dependencies graph is buildable");
    let graph = match get_package_graph(manifest_path) {
        Ok(graph) => graph,
        Err(err) => {
            return writeln!(
                output,
                "Unable to build workspace dependencies graph: {}",
                err
            )
            .map_err(Error::output_error)
            .and_then(|()| Err(err))
        }
    };

    info!("Checking that dependencies are suitable for publishing");
    for (from, links) in graph
        .workspace()
        .iter()
        .flat_map(|package| package.direct_links())
        .filter(|link| !link_is_publishable(link))
        .group_by(PackageLink::from)
        .into_iter()
    {
        debug!("Checking links for package {}", from.name());
        let cargo = read_cargo_toml(from.manifest_path())?;
        for link in links {
            if link.normal().is_present() {
                dependency_has_version(&cargo, &link, DependencyType::Normal).map_err(|err| {
                    writeln!(
                        output,
                        "Dependancy {0} of {1} makes {1} not publishable.",
                        link.to().name(),
                        link.from().name()
                    )
                    .map_err(Error::output_error)
                    .and_then::<(), _>(|_| Err(err))
                    .unwrap_err()
                })?;
            }
            if link.build().is_present() {
                dependency_has_version(&cargo, &link, DependencyType::Build).map_err(|err| {
                    writeln!(
                        output,
                        "Build dependancy {0} of {1} makes {1} not publishable.",
                        link.to().name(),
                        link.from().name()
                    )
                    .map_err(Error::output_error)
                    .and_then::<(), _>(|_| Err(err))
                    .unwrap_err()
                })?;
            }
        }
    }

    Ok(())
}

/// Prepare the Rust workspace for a release.
///
/// Preparing the release updates the version of each crate in the workspace and of
/// the intra-workspace dependencies. The `version` field in the `packages` table of
/// each `Cargo.toml` file in the workspace is set to the supplied version. The
/// `version` field of each dependency or build-dependency that is otherwise
/// identified by a workspace-relative path dependencies is also set to the supplied
/// version (the version filed will be added if it isn't already present).
///
/// This implments the `prepare` step for `sementic-release` for a Cargo-based Rust
/// workspace.
pub fn prepare(
    _output: impl Write,
    manifest_path: Option<impl AsRef<Path>>,
    version: &str,
) -> Result<()> {
    let graph = get_package_graph(manifest_path)?;

    let link_map = graph
        .workspace()
        .iter()
        .flat_map(|package| package.direct_links())
        .filter(|link| !link.dev_only() && link.to().in_workspace())
        .map(|link| (link.from().id(), link))
        .into_group_map();

    for package in graph.workspace().iter() {
        let path = package.manifest_path();
        let mut cargo = read_cargo_toml(path)?;

        set_package_version(&mut cargo, version).map_err(|err| err.into_error(path))?;

        if let Some(links) = link_map.get(package.id()) {
            for link in links {
                if link.normal().is_present() {
                    set_dependencies_version(
                        &mut cargo,
                        version,
                        DependencyType::Normal,
                        link.to().name(),
                    )
                    .map_err(|err| err.into_error(path))?;
                }
                if link.build().is_present() {
                    set_dependencies_version(
                        &mut cargo,
                        version,
                        DependencyType::Build,
                        link.to().name(),
                    )
                    .map_err(|err| err.into_error(path))?;
                }
            }
        }

        write_cargo_toml(package.manifest_path(), cargo)?;
    }

    Ok(())
}

/// List the packages from the workspace in the order of their dependencies.
///
/// The list of pacakges will be written to `output`. If `manifest_path` is provided
/// then it is expected to give the path to the `Cargo.toml` file for the root of the
/// workspace. If `manifest_path` is `None` then `list_packages` will look for the
/// root of the workspace in a `Cargo.toml` file in the current directory.
///
/// This is a debuging aid and does not directly correspond to a sementic release
/// step.
pub fn list_packages(
    mut output: impl Write,
    manifest_path: Option<impl AsRef<Path>>,
) -> Result<()> {
    info!("Building package graph");
    let graph = get_package_graph(manifest_path)?;

    info!("Resolving workspace");
    let set = graph.resolve_workspace();

    info!("Listing packages");
    for package in set.packages(DependencyDirection::Reverse) {
        write!(output, "{}({})", package.name(), package.version()).map_err(Error::output_error)?;
        if package.publish().is_some() {
            write!(output, "--not published").map_err(Error::output_error)?;
        }
        writeln!(output).map_err(Error::output_error)?;
    }

    Ok(())
}

fn get_package_graph(manifest_path: Option<impl AsRef<Path>>) -> Result<PackageGraph> {
    let manifest_path = manifest_path.as_ref().map(|path| path.as_ref());

    let mut command = MetadataCommand::new();
    if let Some(path) = manifest_path {
        command.manifest_path(path);
    }

    debug!("manifest_path: {:?}", manifest_path);

    command.build_graph().map_err(|err| {
        let path = match manifest_path {
            Some(path) => path.to_path_buf(),
            None => env::current_dir()
                .map(|path| path.join("Cargo.toml"))
                .unwrap_or_else(|e| {
                    error!("Unable to get current directory: {}", e);
                    PathBuf::from("unknown manifest")
                }),
        };
        Error::workspace_error(err, path)
    })
}

/// Is the source of the target of a dependencies publishable?
///
/// The target of a dependencies must be available on `crates.io` for the depending
/// package to be publishable. Workspace relative path dependencies will be published
/// before their depended on crates and the dependencies in the depended on crate
/// will have their `version` adjusted so those dependencies will be on `crates.io`
/// by the time the depended on crate is published.
fn target_source_is_publishable(source: PackageSource) -> bool {
    source.is_workspace() || source.is_crates_io()
}

/// Will this link prevent the `link.from()` package from being published.
///
/// `dev-dependencies` links will not prevent publication. For all other links the
/// target of the link must be either already on `crates.io` or it must be a
/// workspace relative path dependency (which will be published first).
fn link_is_publishable(link: &PackageLink) -> bool {
    let result = link.dev_only() || target_source_is_publishable(link.to().source());
    if result {
        trace!(
            "Link from {} to {} is publishable.",
            link.from().name(),
            link.to().name()
        );
    }

    result
}

fn read_cargo_toml(path: impl AsRef<Path>) -> Result<Document> {
    let path = path.as_ref();
    fs::read_to_string(path)
        .map_err(|err| Error::file_read_error(err, path))?
        .parse()
        .map_err(|err| Error::toml_error(err, path))
}

fn write_cargo_toml(path: impl AsRef<Path>, cargo: Document) -> Result<()> {
    let path = path.as_ref();
    fs::write(path, cargo.to_string_in_original_order())
        .map_err(|err| Error::file_write_error(err, path))
}

fn get_top_table<'a>(doc: &'a Document, key: &str) -> Option<&'a Table> {
    doc.as_table().get(key).and_then(Item::as_table)
}

fn get_top_table_mut<'a>(doc: &'a mut Document, key: &str) -> Option<&'a mut Table> {
    doc.as_table_mut().entry(key).as_table_mut()
}

fn table_add_or_update_value(table: &mut Table, key: &str, value: Value) -> Option<()> {
    let entry = table.entry(key);

    if entry.is_none() {
        *entry = Item::Value(value);
        return Some(());
    }

    match entry {
        Item::Value(val) => {
            *val = value;
            Some(())
        }
        _ => None,
    }
}

fn inline_table_add_or_update_value(table: &mut InlineTable, key: &str, value: Value) {
    match table.get_mut(key) {
        Some(ver) => *ver = value,
        None => {
            table.get_or_insert(key, value);
        }
    }
}

fn dependency_has_version(doc: &Document, link: &PackageLink, typ: DependencyType) -> Result<()> {
    let top_key = match typ {
        DependencyType::Normal => "dependencies",
        DependencyType::Build => "build-dependencies",
    };

    trace!(
        "Checking for version key for {} in {} section of {}",
        link.to().name(),
        top_key,
        link.from().name()
    );
    get_top_table(doc, top_key)
        .and_then(|deps| deps.get(link.to().name()))
        .and_then(Item::as_table_like)
        .and_then(|dep| dep.get("version"))
        .map(|_| ())
        .ok_or_else(|| Error::bad_dependency(link, typ))
}

fn set_package_version(doc: &mut Document, version: &str) -> result::Result<(), CargoTomlError> {
    let table =
        get_top_table_mut(doc, "package").ok_or_else(|| CargoTomlError::no_table("package"))?;
    table_add_or_update_value(table, "version", version.into())
        .ok_or_else(|| CargoTomlError::no_value("version"))
}

fn set_dependency_version(table: &mut Table, version: &str, name: &str) -> Option<()> {
    let mut result = Some(());

    if table.contains_key(name) {
        let req = table.entry(name);
        if !req.is_table_like() {
            return None;
        }
        if let Some(req) = req.as_inline_table_mut() {
            inline_table_add_or_update_value(req, "version", version.into());
        }
        if let Some(req) = req.as_table_mut() {
            result = table_add_or_update_value(req, "version", version.into());
        }
    }

    result
}

fn set_dependencies_version(
    doc: &mut Document,
    version: &str,
    typ: DependencyType,
    name: &str,
) -> result::Result<(), CargoTomlError> {
    if let Some(table) = get_top_table_mut(doc, typ.key()) {
        set_dependency_version(table, version, name)
            .ok_or_else(|| CargoTomlError::set_version(name, version))?;
    }

    if let Some(table) = get_top_table_mut(doc, "target") {
        let targets = table.iter().map(|(key, _)| key.to_owned()).collect_vec();

        for target in targets {
            if let Some(target_deps) = table.entry(&target)
                .as_table_mut()
                .and_then(|inner| inner[typ.key()].as_table_mut())
            {
                set_dependency_version(target_deps, version, name)
                    .ok_or_else(|| CargoTomlError::set_version(name, version))?;
            }
        }
    };

    Ok(())
}

/// The type of a dependency for a package.
#[derive(Debug)]
pub enum DependencyType {
    /// A normal dependency (i.e. "dependencies" section of `Cargo.toml`).
    Normal,

    /// A build dependency (i.e. "build-dependencies" section of `Cargo.toml`).
    Build,
}

impl DependencyType {
    fn key(&self) -> &str {
        use DependencyType::*;

        match self {
            Normal => "dependencies",
            Build => "build-dependencies",
        }
    }
}

impl fmt::Display for DependencyType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use DependencyType::*;

        match self {
            Normal => write!(f, "Dependancy"),
            Build => write!(f, "Build dependency"),
        }
    }
}
