#chg-compatible

test sparse

  $ enable sparse rebase
  $ hg init myrepo
  $ cd myrepo

  $ echo z > file.txt
  $ mkdir noinc
  $ mkdir -p inc/exc
  $ echo z > noinc/file.txt
  $ echo z > inc/file.txt
  $ echo z > inc/exc/incfile.txt
  $ echo z > inc/exc/excfile.txt
  $ cat > base.sparse <<EOF
  > [include]
  > glob:*.sparse
  > path:file.txt
  > path:inc/
  > [exclude]
  > path:inc/exc
  > EOF
  $ cat > main.sparse <<EOF
  > %include base.sparse
  > [include]
  > path:inc/exc/incfile.txt
  > EOF
  $ hg ci -Aqm 'initial'
  $ hg sparse enable main.sparse

# Verify inc/exc/incfile.txt is not included.
  $ ls -R | grep -v :
  base.sparse
  file.txt
  inc
  main.sparse
  
  file.txt

# Upgrade main.sparse to v2
  $ cat > main.sparse <<EOF
  > [metadata]
  > version: 2
  > %include base.sparse
  > [include]
  > inc/exc/incfile.txt
  > EOF
  $ hg commit -qm "v2 sparse"

# Verify inc/exc/incfile.txt is now included.
  $ ls -R | grep -v :
  base.sparse
  file.txt
  inc
  main.sparse
  
  exc
  file.txt
  
  incfile.txt

  $ hg debugsparseprofilev2 main.sparse
  V1 includes 4 files
  V2 includes 5 files
  + inc/exc/incfile.txt

