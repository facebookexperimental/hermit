# Hermit System Achitecture

Hermit is built out of a number of components found within the hermetic_infrastructure folder,
corresponding to the directories `hermit`, `detcore`, and `reverie`.

[Reverie](https://github.com/facebookexperimental/reverie) is the program-instrumentation layer, responsible for
intercepting all system calls (and other events) from the guest and allowing injection of syscalls
into the underlying Linux kernel.  Each Reverie "tool" (or "plugin") is effectively a custom
operating system, but it is at liberty to use the underlying Linux kernel rather than reimplement
everything from scratch. (This arrangement is elsewhere called a [Sentry](https://gvisor.dev/docs/).)

{{#plantuml:
@startuml
package "Hermit" {
  node Detcore
  node Reverie
  Detcore - Reverie
}
node "Linux Kernel" as linux
node (Guest Program) as guest
guest --> Reverie
Reverie --> linux
@enduml
}}

The command-line executable `hermit` is by instantiating Reverie with the Detcore tool.  The Hermit
binary has the job of setting up the container, and then Detcore takes over as the guest runs,
intercepting its runtime events, maintaining state between events, and managing interaction with the
underlying kernel.

## The global and local components of Detcore

(Under Construction)

{{#plantuml:
@startuml
node (Guest Events) as guest
package "Detcore" {
  node tool_global
  node tool_local
  tool_global <-- tool_local: "rpc calls"
}
' guest --> tool_local : "trap syscalls, etc"
note bottom of guest
  trap syscalls, rdtsc/cpuid, signals,
  even timer-preemption events
end note
guest --> tool_local
@enduml
}}
