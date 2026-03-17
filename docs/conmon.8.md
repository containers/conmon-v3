# NAME

conmon - OCI container monitor used by Podman and CRI-O

# SYNOPSIS

**conmon** [OPTIONS] -c _CID_ --runtime _PATH_

Create/run, exec into, or restore a container while handling logging, exit status reporting, and lifecycle integration for higher-level tools.

# DESCRIPTION

**conmon** is a small, feature-focused OCI container monitor used primarily by
Podman and CRI-O. It launches an OCI runtime (such as **runc**), keeps track of
the container process, manages attach sockets, handles stdio, writes exit
status files, and forwards container logs to a pluggable logging backend.

The same command-line interface supports multiple logical modes:

- **Create / run (default)**: start a container using an OCI bundle.
- **Exec**: execute an additional process in an existing container.
- **Restore**: restore a container from a checkpoint.
- **Version**: print the conmon version and exit.

The mode is selected using flags such as **--exec**, **--restore**, and
**--version**, not by subcommands.

# OPTIONS

Unless stated otherwise, boolean options are disabled by default and become
enabled when specified. Options marked "(multiple)" may be given more than
once.

## General options

**--version**

: Print the conmon version and exit.

**--api-version**=_INT_

: Conmon API version to use. If omitted, defaults to 0. API version affects
  exec/attach behavior: attaching to an exec session (**--exec-attach**) is
  only allowed when **--api-version** is at least 1.

**-c**, **--cid**=_STRING_

: Container ID. This uniquely identifies the container instance and is
  required for all modes other than **--version**. If missing, conmon fails
  with "Container ID not provided. Use --cid".

**-u**, **--cuuid**=_STRING_

: Container UUID. Required for create/run and restore, and for exec with
  non-legacy API versions. It may be omitted only for legacy exec mode when
  **--api-version** is less than 1 and **--exec** is used. Otherwise conmon
  fails with "Container UUID not provided. Use --cuuid".

**-n**, **--name**=_STRING_

: Human-readable container name. This is used in logging metadata and may
  appear in log records depending on the configured log plugin.

**-b**, **--bundle**=_PATH_

: Location of the OCI bundle directory. If not specified, conmon defaults to
  the current working directory. The bundle is also used as the default
  location for the conmon debug log file (see **ENVIRONMENT**).

**-r**, **--runtime**=_PATH_

: Path to the OCI runtime binary used to manage the container (for example,
  **/usr/bin/runc**). This option is required for all modes other than
  **--version**. The path must refer to an executable file; otherwise conmon
  fails with "Runtime path … is not valid".

**--runtime-arg**=_ARG_ (multiple)

: Additional argument to pass to the runtime for all operations. Can be
  specified multiple times. Values may begin with **-**.

**--runtime-opt**=_ARG_ (multiple)

: Additional options passed to the runtime for restore or exec operations.
  Can be specified multiple times. Values may begin with **-**.

**--persist-dir**=_PATH_

: Persistent directory for the container. conmon writes exit status files here
  so higher-level tools can detect container exit using inotify or directory
  polling.

**--socket-dir-path**=_PATH_

: Directory where attach sockets for the container are created. If not
  specified, defaults to **/var/run/crio**.

**--full-attach**

: Use the full path to the attach socket instead of truncating it based on
  **--socket-dir-path**. When specified, conmon ignores **--socket-dir-path**
  for the attach socket path computation.

**--sdnotify-socket**=_PATH_

: Path to the host's systemd sd-notify socket. When set, conmon relays
  sd-notify messages from the container to this socket.

**--seccomp-notify-socket**=_PATH_

: Path to the socket on which the seccomp notification file descriptor is
  received.

**--seccomp-notify-plugins**=_STRING_

: Comma-separated list or specification of plugins that manage seccomp
  notifications. The exact semantics are defined by the configured plugins.

**--timeout**, **-T**=_SECONDS_

: Kill the container after the specified timeout in seconds. If unset, conmon
  does not impose a timeout on the container.

**--sync**

: Keep the main conmon process as the direct parent of the container by only
  forking once. This is mainly useful for debugging or special integration
  scenarios.

**--syslog**

: Log to syslog. This is intended for use with the cgroupfs cgroup manager.
  It controls how conmon itself logs; it is distinct from the container log
  plugin configured via **--log-path**.

**-s**, **--systemd-cgroup**

: Enable systemd-based cgroup management instead of cgroupfs when launching
  or restoring a container. This is passed through as part of the runtime
  configuration.

**--no-new-keyring**

: Do not create a new session keyring for the container.

**--no-pivot**

: Disable **pivot_root(2)** and use alternative root switching mechanisms.

**--replace-listen-pid**

: Replace an existing listen PID with the OCI runtime PID, when set. This is
  used by some higher-level integrations for PID tracking.

**--stdin**, **-i**

: Open a pipe to pass standard input to the container.

**--terminal**, **-t**

: Allocate a pseudo-TTY for the container's stdin/stdout/stderr.

## PID files and exit handling

**--conmon-pidfile**, **-P**=_PATH_

: Write the PID of the conmon monitor process to the given file.

**--container-pidfile**, **-p**=_PATH_

: Write the PID of the initial process inside the container to the given
  file. If this option is not provided, conmon defaults to a file named
  **pidfile-**_CID_ in the current working directory.

**--pidfile**=_PATH_ (deprecated)

: Deprecated PID file option kept for backward compatibility. Hidden from the
  built-in help output; prefer **--conmon-pidfile** or **--container-pidfile**
  instead.

**--exit-dir**=_PATH_

: Path to the directory where exit files are written. These files allow
  higher-level tools such as Podman or CRI-O to detect container exit and
  read exit status.

**--exit-command**=_PATH_

: Path to an external program to execute when the container terminates. The
  exit command receives arguments from **--exit-command-arg** and runs after
  exit files are written.

**--exit-command-arg**=_ARG_ (multiple)

: Additional argument to pass to the program specified by **--exit-command**.
  May be specified multiple times. Values may begin with **-**.

**--exit-delay**=_SECONDS_

: Delay, in seconds, before invoking the exit command after container exit.

## Logging options

**--log-level**=_LEVEL_

: Set the minimum log level for conmon's own debug logging (separate from
  container logs). The supported values follow the typical logging levels
  (for example, **debug**, **info**, **warn**, **error**); invalid values are
  treated according to the internal logger defaults.

**-l**, **--log-path**=_SPEC_ (multiple)

: Configure container logging destination and plugin. This option can be
  specified multiple times and is required unless **--version** is used.
  Each value has one of the following forms:

  * `plugin:path` - Use the given plugin and log to *path*.
    Dashes in the plugin name are normalized to underscores (for example,
    `k8s-file` becomes `k8s_file`).
  * `journald` - Use the **journald** logging plugin.
  * `passthrough` - Use the **passthrough** logging plugin (no additional file
    path).
  * `path` - Any other non-empty value is treated as a file path for the
    default **file** logging plugin.

  If no usable **--log-path** value is provided, conmon exits with
  "Log driver not provided. Use --log-path".

**--log-size-max**=_BYTES_

: Maximum size in bytes of a single container log file before rotation or
  truncation is considered. If unset, the default is 0 (no size-based limit).

**--log-global-size-max**=_BYTES_

: Maximum total size in bytes of all log files managed by the log plugin. If
  unset, the default is 0 (no global size limit).

**--log-tag**=_STRING_

: Additional tag to include in log records.

**--log-label**=_STRING_ (multiple)

: Additional label to include in log records. Can be specified multiple times.

**--log-allowlist-dir**=_PATH_ (multiple)

: Allowed log directory. Can be specified multiple times. When set, the log
  plugin restricts log writes to the given directories. If omitted, no
  allowlist restriction is applied.

**--no-container-partial-message**

: Do not set **CONTAINER_PARTIAL_MESSAGE=true** for partial log lines when
  using the **journald** log driver. If specified with a non-**journald**
  plugin, conmon logs a warning and the option has no effect.

**--no-sync-log**

: Do not manually call sync on logs after container shutdown. This can
  improve performance at the cost of potentially losing log records in the
  event of a sudden system failure.

**--log-rotate**

: Enable log rotation instead of truncation when **--log-size-max** is
  reached. Rotation requires **--log-max-files** to be at least 1.

**--log-max-files**=_N_

: Number of backup log files to keep when rotation is enabled. The default is
  1. The value must be non-negative; values greater than **INT32_MAX** are
  rejected. If **--log-rotate** is specified and **--log-max-files** is 0,
  conmon exits with "log-max-files must be at least 1 when log-rotate is
  enabled".

## Exec and restore options

These options select and configure the exec and restore modes. They cannot be
combined arbitrarily.

**-e**, **--exec**

: Exec a command into a running container, instead of creating a new
  container. When **--exec** is set:

  * **--exec-process-spec** is required.
  * **--cuuid** is required unless **--api-version** is less than 1 (legacy
    exec API).
  * **--restore** must not be set.

**--exec-process-spec**=_PATH_

: Path to the OCI process specification (typically a JSON file) describing the
  process to execute inside the container. Required when **--exec** is used.
  If missing, conmon exits with "Exec process spec path not provided. Use
  --exec-process-spec".

**--exec-attach**

: Attach to an exec session. This option is only valid when **--exec** is
  also specified, and when **--api-version** is at least 1. If used without
  **--exec**, conmon fails with "Attach can only be specified with exec". If
  used with **--api-version** less than 1, conmon fails with "Attach can only
  be specified for a non-legacy exec session".

**--restore**=_PATH_

: Restore a container from a previously created checkpoint at the specified
  path. When **--restore** is used:

  * **--exec** must not be set (conmon rejects configurations that use both).
  * **--cid**, **--cuuid**, and **--runtime** are still required.

**--restore-arg**=_ARG_ (deprecated, multiple)

: Additional argument to pass to the restore command. Deprecated and hidden
  from the built-in help, but still parsed for backward compatibility. Values
  may begin with **-**.

## Attach and I/O behavior

**--leave-stdin-open**

: Leave standard input open when the attached client disconnects, instead of
  closing the container's stdin.

## Mode selection summary

conmon selects its internal command mode based on the provided options:

- **Version mode**
  * Selected when **--version** is specified.
  * Prints version information and exits immediately.

- **Restore mode**
  * Selected when **--restore** is provided (and **--exec** is not).
  * Restores a container from the specified checkpoint path.

- **Exec mode**
  * Selected when **--exec** is set (and **--restore** is not).
  * Runs an additional process in an existing container using
    **--exec-process-spec**, optionally with **--exec-attach**.

- **Create / run mode**
  * Default when neither **--exec** nor **--restore** nor **--version** is
    set.
  * Creates and runs a new container using the OCI bundle at **--bundle** (or
    the current working directory by default).

# ENVIRONMENT

The following environment variables affect conmon's own debug logging. They do
not control the container log plugin, which is configured via **--log-path**
and related CLI options.

**CONMON_LOG_PATH**

: Path to the debug log file for conmon itself. If not set, and **--bundle**
  is provided, conmon defaults to writing its debug log to
  _BUNDLE_/**conmon-debug.log**. If neither **CONMON_LOG_PATH** nor
  **--bundle** is set, no debug log file path is configured.

**CONMON_LOG_LEVEL**

: Minimum log level for conmon's internal debug logging. If not set or set to
  an invalid value, conmon defaults to a debug-level log filter.

# EXIT STATUS

On success, conmon exits with the exit status of the container or exec
process, depending on the active mode. On internal errors (invalid arguments,
runtime failures, logging misconfiguration, and similar), conmon prints an
error message of the form:

> conmon: _MESSAGE_

and exits with an appropriate non-zero status code.

In all normal cases, conmon:

- Writes exit files into the directory specified by **--persist-dir** and/or
  **--exit-dir**, so higher-level tools can detect container exit and read the
  exit status.
- Flushes any buffered container log output in the configured log plugin
  before exiting.
- Optionally runs the program specified by **--exit-command**, passing any
  **--exit-command-arg** values, after an optional **--exit-delay**.

# EXAMPLES

Create and run a container using an OCI bundle in the current directory,
logging to a file:

    conmon \
      --cid mycid \
      --cuuid 123e4567-e89b-12d3-a456-426655440000 \
      --runtime /usr/bin/runc \
      --log-path /var/log/containers/mycid.log

Exec a process in a running container with attach, using API version 1 and a
process spec:

    conmon \
      --cid mycid \
      --cuuid 123e4567-e89b-12d3-a456-426655440000 \
      --runtime /usr/bin/runc \
      --api-version 1 \
      --exec \
      --exec-process-spec /run/mycid/exec-spec.json \
      --exec-attach \
      --log-path passthrough

Restore a container from a checkpoint with systemd cgroups enabled:

    conmon \
      --cid mycid \
      --cuuid 123e4567-e89b-12d3-a456-426655440000 \
      --runtime /usr/bin/runc \
      --restore /var/lib/checkpoints/mycid \
      --systemd-cgroup \
      --log-path file:/var/log/containers/mycid.log

# SEE ALSO

**runc(8)**, **podman(1)**, **crio(8)**

