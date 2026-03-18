%global debug_package %{nil}

%global provider github
%global provider_tld com
%global project containers
%global repo conmon-v3
%global provider_prefix %{provider}.%{provider_tld}/%{project}/%{repo}
%global import_path %{provider_prefix}
%global git0 https://%{import_path}
%global commit0 021359ce8cf2edee5628206c14fc1e5165560f6d
%global shortcommit0 %(c=%{commit0}; echo ${c:0:8})

Name: %{repo}
Version: 3.0.0
Release: %autorelease
Summary: OCI container runtime monitor (v3)
License: Apache-2.0
URL: %{git0}
Source0: conmon-v3-3.0.0.tar.gz
ExclusiveArch: aarch64 %{arm} ppc64le s390x x86_64

BuildRequires: cargo
BuildRequires: rust
BuildRequires: git
BuildRequires: glib2-devel
BuildRequires: glibc-devel
BuildRequires: libseccomp-devel
BuildRequires: pkgconfig
BuildRequires: systemd-devel
BuildRequires: go-md2man

Requires: glib2
Requires: systemd-libs
Requires: libseccomp

%description
%{summary}.

%prep
%autosetup -Sgit -n conmon-v3-2.2.1

%build
%{__make} release
%{__make} docs

%install
mkdir -p %{buildroot}%{_bindir}
install -m 0755 target/release/conmon %{buildroot}%{_bindir}/conmon-v3
mkdir -p %{buildroot}%{_mandir}/man8
install -m 0644 docs/conmon.8 %{buildroot}%{_mandir}/man8/conmon.8

#define license tag if not already defined
%{!?_licensedir:%global license %doc}

%files
%license LICENSE
%doc README.md
%{_bindir}/conmon-v3
%{_mandir}/man8/conmon.8*

%changelog
%autochangelog
