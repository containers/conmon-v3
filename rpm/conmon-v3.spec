# Generate Source1 after a release tag:
#   make vendor-tarball
# Upload vendor-tarball/conmon-v3-v%%{version}-vendor.tar.gz to GitHub Releases alongside the sources.

%bcond_without check

%global forgeurl https://github.com/containers/conmon-v3
%global repo conmon-v3
# Cargo.toml version (hyphen); RPM Version uses tilde for ordering.
%global upstream_version 3.0.0-dev

# Build environment toggles.
%global conmon_is_rhel 0%{?rhel:1}
%global conmon_is_copr 0%{?copr_username:1}
%global conmon_is_packit_no_vendor 0%{?packit_no_vendor_tarball:1}
%global conmon_use_rust_toolset_macros %{conmon_is_rhel}
%global conmon_use_vendor_tarball 0%{?fedora}%{?rhel} && (0%{?fedora} || 0%{?rhel} >= 10) && !%{conmon_is_copr} && !%{conmon_is_packit_no_vendor}

Name:           %{repo}
Version:        %(echo %{upstream_version} | sed 's/-/~/')
Release:        %autorelease
Summary:        OCI container runtime monitor (v3)

SourceLicense:  Apache-2.0
License:        %{shrink:
    Apache-2.0 AND
    MIT AND
    Unicode-3.0 AND
    BSL-1.0 AND
    LGPL-2.1-or-later WITH GCC-exception-2.0 AND
    Unlicense
}

URL:            %{forgeurl}
Source0:        %{forgeurl}/archive/v%{upstream_version}/%{repo}-%{upstream_version}.tar.gz
Source1:        %{forgeurl}/releases/download/v%{upstream_version}/%{repo}-v%{upstream_version}-vendor.tar.gz

%if %{defined golang_arches_future}
ExclusiveArch: %{golang_arches_future}
%else
ExclusiveArch: aarch64 %{arm} ppc64le s390x x86_64
%endif

%if %{conmon_use_rust_toolset_macros}
BuildRequires:  rust-toolset
%else
BuildRequires:  cargo-rpm-macros
%endif
BuildRequires:  cargo
BuildRequires:  git
BuildRequires:  glibc-devel
BuildRequires:  libseccomp-devel
BuildRequires:  pkg-config
BuildRequires:  systemd-devel
BuildRequires:  go-md2man

Requires:       systemd-libs
Requires:       libseccomp

%description
%{summary}.

%prep
%autosetup -Sgit -n %{repo}-%{upstream_version} -p1
%if %{defined copr_username} || %{defined packit_no_vendor_tarball}
%cargo_prep -N
# %%cargo_prep always sets [net] offline = true (Koji); Copr/Packit need crates.io with enable_net.
sed -i 's/^offline = true$/offline = false/' .cargo/config.toml
%else
tar fx %{SOURCE1}
%if 0%{?fedora} || 0%{?rhel} >= 10
%cargo_prep -v vendor
%else
%cargo_prep -V 1
%endif
%endif

%build
%cargo_build
%if %{conmon_use_vendor_tarball}
%{cargo_license_summary}
%{cargo_license} > LICENSE.dependencies
%cargo_vendor_manifest
%endif
%{__make} -C docs docs

%install
install -Dpm 0755 target/rpm/conmon %{buildroot}%{_bindir}/%{name}
install -d %{buildroot}%{_mandir}/man8
install -m 0644 docs/conmon.8 %{buildroot}%{_mandir}/man8/%{name}.8

%check
%if %{with check}
%cargo_test
%endif

%files
%license LICENSE
%if %{conmon_use_vendor_tarball}
%license LICENSE.dependencies
%license cargo-vendor.txt
%endif
%doc README.md
%{_bindir}/%{name}
%{_mandir}/man8/%{name}.8*

%changelog
%autochangelog
