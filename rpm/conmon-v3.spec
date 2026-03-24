%global debug_package %{nil}

%global provider github
%global provider_tld com
%global project containers
%global repo conmon-v3
%global provider_prefix %{provider}.%{provider_tld}/%{project}/%{repo}
%global import_path %{provider_prefix}
%global git0 https://%{import_path}

Name: %{repo}
Version: 3.0.0
Release: %autorelease
Summary: OCI container runtime monitor (v3)
License: Apache-2.0
URL: %{git0}
Source0: conmon-v3-3.0.0.tar.gz
%if %{defined golang_arches_future}
ExclusiveArch: %{golang_arches_future}
%else
ExclusiveArch: aarch64 %{arm} ppc64le s390x x86_64
%endif

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
%autosetup -Sgit -n conmon-v3-3.0.0

%build
%{__make} release

%install
%{__make} DESTDIR=%{buildroot} PREFIX=%{_prefix} install

#define license tag if not already defined
%{!?_licensedir:%global license %doc}

%files
%license LICENSE
%doc README.md
%{_bindir}/%{name}
%{_mandir}/man8/%{name}.8*

%changelog
%autochangelog
