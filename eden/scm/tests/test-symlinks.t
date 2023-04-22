  $ setconfig experimental.windows-symlinks=True
#if no-windows
#endif
  $ hg add -q
  $ hg diff --git
  diff --git a/a/b/c/demo b/a/b/c/demo
  new file mode 120000
  --- /dev/null
  +++ b/a/b/c/demo
  @@ -0,0 +1,1 @@
  +/path/to/symlink/source
  \ No newline at end of file
  $ hg commit -m 'add symlink in a/b/c subdir'
  $ hg show --stat --git
  commit:      7c0e359fc055
  user:        test
  date:        Thu Jan 01 00:00:00 1970 +0000
  files:       a/b/c/demo
  description:
  add symlink in a/b/c subdir
  
  
   a/b/c/demo |  1 +
   1 files changed, 1 insertions(+), 0 deletions(-)
#if no-windows