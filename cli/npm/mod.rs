// Copyright 2018-2023 the Deno authors. All rights reserved. MIT license.

mod cache_dir;
mod managed;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use deno_ast::ModuleSpecifier;
use deno_core::error::AnyError;
use deno_core::url::Url;
use deno_graph::NpmPackageReqResolution;
use deno_npm::resolution::PackageReqNotFoundError;
use deno_runtime::deno_node::NpmResolver;
use deno_semver::npm::NpmPackageNvReference;
use deno_semver::npm::NpmPackageReqReference;
use deno_semver::package::PackageNv;
use deno_semver::package::PackageReq;

pub use self::cache_dir::NpmCacheDir;
pub use self::managed::CliNpmResolverManagedCreateOptions;
pub use self::managed::CliNpmResolverManagedPackageJsonInstallerOption;
pub use self::managed::CliNpmResolverManagedSnapshotOption;
pub use self::managed::ManagedCliNpmResolver;

pub enum CliNpmResolverCreateOptions {
  Managed(CliNpmResolverManagedCreateOptions),
  // todo(dsherret): implement this
  #[allow(dead_code)]
  Byonm,
}

pub async fn create_cli_npm_resolver_for_lsp(
  options: CliNpmResolverCreateOptions,
) -> Arc<dyn CliNpmResolver> {
  use CliNpmResolverCreateOptions::*;
  match options {
    Managed(options) => {
      managed::create_managed_npm_resolver_for_lsp(options).await
    }
    Byonm => todo!(),
  }
}

pub async fn create_cli_npm_resolver(
  options: CliNpmResolverCreateOptions,
) -> Result<Arc<dyn CliNpmResolver>, AnyError> {
  use CliNpmResolverCreateOptions::*;
  match options {
    Managed(options) => managed::create_managed_npm_resolver(options).await,
    Byonm => todo!(),
  }
}

pub enum InnerCliNpmResolverRef<'a> {
  Managed(&'a ManagedCliNpmResolver),
  #[allow(dead_code)]
  Byonm(&'a ByonmCliNpmResolver),
}

pub trait CliNpmResolver: NpmResolver {
  fn into_npm_resolver(self: Arc<Self>) -> Arc<dyn NpmResolver>;

  fn clone_snapshotted(&self) -> Arc<dyn CliNpmResolver>;

  fn root_dir_url(&self) -> &Url;

  fn as_inner(&self) -> InnerCliNpmResolverRef;

  fn as_managed(&self) -> Option<&ManagedCliNpmResolver> {
    match self.as_inner() {
      InnerCliNpmResolverRef::Managed(inner) => Some(inner),
      InnerCliNpmResolverRef::Byonm(_) => None,
    }
  }

  fn node_modules_path(&self) -> Option<PathBuf>;

  /// Checks if the provided package req's folder is cached.
  fn is_pkg_req_folder_cached(&self, req: &PackageReq) -> bool;

  /// Resolves a package requirement for deno graph. This should only be
  /// called by deno_graph's NpmResolver or for resolving packages in
  /// a package.json
  fn resolve_npm_for_deno_graph(
    &self,
    pkg_req: &PackageReq,
  ) -> NpmPackageReqResolution;

  fn resolve_pkg_nv_ref_from_pkg_req_ref(
    &self,
    req_ref: &NpmPackageReqReference,
  ) -> Result<NpmPackageNvReference, PackageReqNotFoundError>;

  /// Resolve the root folder of the package the provided specifier is in.
  ///
  /// This will error when the provided specifier is not in an npm package.
  fn resolve_pkg_folder_from_specifier(
    &self,
    specifier: &ModuleSpecifier,
  ) -> Result<Option<PathBuf>, AnyError>;

  fn resolve_pkg_folder_from_deno_module_req(
    &self,
    req: &PackageReq,
  ) -> Result<PathBuf, AnyError>;

  fn resolve_pkg_folder_from_deno_module(
    &self,
    nv: &PackageNv,
  ) -> Result<PathBuf, AnyError>;

  /// Gets the state of npm for the process.
  fn get_npm_process_state(&self) -> String;

  // todo(#18967): should instead return a hash state of the resolver
  // or perhaps this could be non-BYONM only and byonm always runs deno check
  fn package_reqs(&self) -> HashMap<PackageReq, PackageNv>;
}

// todo(#18967): implement this
pub struct ByonmCliNpmResolver;
