# conmon-v3

An OCI container runtime monitor.

Conmon is a monitoring program and communication tool between a
container manager (like [Podman](https://podman.io/) or
[CRI-O](https://cri-o.io/)) and an OCI runtime (like
[runc](https://github.com/opencontainers/runc) or
[crun](https://github.com/containers/crun)) for a single container.

Upon being launched, conmon (usually) double-forks to daemonize and detach from the
parent that launched it. It then launches the runtime as its child. This
allows the managing process to exit in the foreground while still being able to
watch over and connect to the child process (the container).

While the container runs, conmon does two things:

- Provides a socket for attaching to the container, holding open the
  container's standard streams and forwarding them over the socket.
- Writes the contents of the container's streams to a log file (or to
  the systemd journal) so they can be read after the container's
  death.

Finally, upon the container's death, conmon will record its exit time and
code to be read by the managing programs.

Written in Rust and designed to have a low memory footprint, conmon is
intended to be run by a container management library. Essentially, conmon
is the smallest daemon a container can have.

In most cases, conmon should be packaged with your favorite container
manager. However, if you'd like to try building it from source, follow
the steps below.

## Run Podman with conmon-v3

To test conmon-v3 on Fedora, CentOS, or RHEL, do the following:

```shell
$ sudo dnf copr enable rhcontainerbot/podman-next
$ sudo dnf install conmon-v3
$ sudo dnf copr disable rhcontainerbot/podman-next
```

It is important to disable the COPR repository after installation; otherwise, any future `dnf update` will install the latest unreleased versions of Podman and other tools.

To update conmon-v3 to the latest version, run:

```shell
$ sudo dnf --enablerepo=copr:copr.fedorainfracloud.org:rhcontainerbot:podman-next update conmon-v3
```

To switch Podman to conmon-v3, edit `/usr/share/containers/containers.conf` and change the `conmon_path` option as follows:

```
conmon_path = [
  "/usr/bin/conmon-v3"
]
```

To verify that Podman is using conmon-v3, run `podman info` as follows:

```
$ podman info|grep conmon
  conmon:
    package: conmon-v3-3.0.0-1.20260324101217195131.main.fc43.x86_64
    path: /usr/bin/conmon-v3
    version: 'conmon version 3.0.0-dev, commit: <commit_hash>'
```

## Dependencies

These dependencies are required for the build:

### Fedora, CentOS, RHEL, and related distributions:

```shell
sudo yum install -y \
  rust \
  make \
  cargo
```

### Debian, Ubuntu, and related distributions:

```shell
sudo apt-get install \
  rust \
  make \
  cargo
```

## Build

Once all the dependencies are installed:

```shell
make
```

There is one option for installation, depending on your environment.
Each can have the PREFIX overridden. The PREFIX defaults to `/usr/local`
for most Linux distributions.

- `make install` installs to `$PREFIX/bin`, adding conmon to your PATH.

Note: to run conmon, you'll also need to have an OCI-compliant runtime
installed, like [runc](https://github.com/opencontainers/runc) or
[crun](https://github.com/containers/crun).
