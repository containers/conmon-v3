# conmon-v3

An OCI container runtime monitor.

Conmon is a monitoring program and communication tool between a
container manager (like [Podman](https://podman.io/) or
[CRI-O](https://cri-o.io/)) and an OCI runtime (like
[runc](https://github.com/opencontainers/runc) or
[crun](https://github.com/containers/crun)) for a single container.

Upon being launched, conmon (usually) double-forks to daemonize and detach from the
parent that launched it. It then launches the runtime as its child. This
allows managing processes to die in the foreground, but still be able to
watch over and connect to the child process (the container).

While the container runs, conmon does two things:

- Provides a socket for attaching to the container, holding open the
  container's standard streams and forwarding them over the socket.
- Writes the contents of the container's streams to a log file (or to
  the systemd journal) so they can be read after the container's
  death.

Finally, upon the containers death, conmon will record its exit time and
code to be read by the managing programs.

Written in C and designed to have a low memory footprint, conmon is
intended to be run by a container managing library. Essentially, conmon
is the smallest daemon a container can have.

In most cases, conmon should be packaged with your favorite container
manager. However, if you'd like to try building it from source, follow
the steps below.

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

There are three options for installation, depending on your environment.
Each can have the PREFIX overridden. The PREFIX defaults to `/usr/local`
for most Linux distributions.

- `make install` installs to `$PREFIX/bin`, for adding conmon to the
  path.

Note, to run conmon, you'll also need to have an OCI compliant runtime
installed, like [runc](https://github.com/opencontainers/runc) or
[crun](https://github.com/containers/crun).
