These tests are not implicitly wrapped with a call to `hermit run`.
Rather, they are standalone shell scripts that invoke hermit themselves.

By convention, they expect to be passed a path to the hermit binary as
their first argument.  Otherwise, they will simply assume `hermit` is
on the path.
