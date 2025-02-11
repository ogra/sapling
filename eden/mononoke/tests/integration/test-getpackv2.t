# Copyright (c) Meta Platforms, Inc. and affiliates.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License found in the LICENSE file in the root
# directory of this source tree.

  $ . "${TEST_FIXTURES}/library.sh"
  $ setconfig ui.ignorerevnum=false
  $ setconfig remotefilelog.write-hgcache-to-indexedlog=False remotefilelog.write-local-to-indexedlog=False

setup configuration
  $ setup_common_config
  $ cd $TESTTMP

setup repo

  $ hg init repo-hg

setup hg server repo
  $ cd repo-hg
  $ setup_hg_server
  $ cd $TESTTMP

setup client repo2
  $ hgclone_treemanifest ssh://user@dummy/repo-hg repo2 --noupdate -q
  $ hgclone_treemanifest ssh://user@dummy/repo-hg repo3 --noupdate -q
  $ cd repo2
  $ setup_hg_client

make a few commits on the server
  $ cd $TESTTMP/repo-hg
  $ drawdag <<EOF
  > C
  > |
  > B
  > |
  > A
  > EOF

create master bookmark

  $ hg bookmark master_bookmark -r tip

blobimport them into Mononoke storage and start Mononoke
  $ cd ..
  $ blobimport repo-hg/.hg repo

start mononoke

  $ start_and_wait_for_mononoke_server
Pull from Mononoke
  $ cd repo2
  $ setconfig remotefilelog.fetchpacks=True
  $ setconfig remotefilelog.getpackversion=2
  $ setconfig extensions.pushrebase=
  $ hgmn pull -q --config ui.disable-stream-clone=true
  warning: stream clone is disabled

Make sure that cache is empty
  $ ls $TESTTMP/cachepath/repo/packs/manifests

  $ hgmn prefetch -r "min(all())" -r1 --debug 2>&1 | grep "getpackv2 command"
  sending getpackv2 command

Make sure that `hg update` succeeds
  $ hg up --config paths.default=badpath 1
  2 files updated, 0 files merged, 0 files removed, 0 files unresolved

Create new commit that modifies A
  $ hgmn up -q tip
  $ echo AA > A && hg ci -m 'AA'
  $ hgmn push -r . --to master_bookmark -q

Go to repo3 and prefetch both revisions that modified file A.
Then make sure update succeeds
  $ cd $TESTTMP/repo3
  $ setconfig remotefilelog.fetchpacks=True
  $ setconfig remotefilelog.getpackversion=2
  $ hgmn pull -q --config ui.disable-stream-clone=true
  warning: stream clone is disabled
  $ hgmn prefetch -r "min(all())" -r 3 --debug 2>&1 | grep "getpackv2 command"
  sending getpackv2 command
  $ hg up --config paths.default=badpath 0
  1 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ cat A
  A (no-eol)
  $ hg log -f A
  commit:      426bada5c675
  user:        test
  date:        Thu Jan 01 00:00:00 1970 +0000
  summary:     A
  
  $ hg up --config paths.default=badpath 3
  3 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ cat A
  AA
  $ hg log -f A
  commit:      be4e0feadad6
  bookmark:    master_bookmark
  user:        test
  date:        Thu Jan 01 00:00:00 1970 +0000
  summary:     AA
  
  commit:      426bada5c675
  user:        test
  date:        Thu Jan 01 00:00:00 1970 +0000
  summary:     A
  
Rename a file and then prefetch it
  $ cd $TESTTMP/repo2
  $ hgmn up -q tip
  $ hg mv A AA
  $ hg ci -m 'rename A to AA'
  $ hgmn push -r . --to master_bookmark
  pushing to mononoke://$LOCALIP:$LOCAL_PORT/repo
  searching for changes
  adding changesets
  adding manifests
  adding file changes
  $ cd $TESTTMP/repo3
  $ hgmn pull -q
  $ hgmn prefetch -r 4 --debug 2>&1 | grep "getpackv2 command"
  sending getpackv2 command
  $ hg debugdatapack --node 5abbc96341e3bb0cdfc5c54599ee869e2ffa573f $TESTTMP/cachepath/repo3/packs/ee71793980651ba90038f48b623b83d4f3c8585a.dataidx
  $TESTTMP/cachepath/repo3/packs/ee71793980651ba90038f48b623b83d4f3c8585a:
  \x01 (esc)
  copy: A
  copyrev: bb3317de12b1232de9b883d9026c1ffa9291e3e6
  \x01 (esc)
  AA
  $ hg up -q 4 --config paths.default=badpath
  $ hg st --change . -C --config paths.default=badpath
  A AA
    A
  R A

Make sure the push succeeds - we had a problem when an incorrect delta was
generated because copy metadata wasn't added
  $ echo B > AA
  $ hg ci -m 'commit on top of a rename'
  $ hgmn push -r . --to master_bookmark --config extensions.remotenames=
  pushing rev 0ce8239858c4 to destination mononoke://$LOCALIP:$LOCAL_PORT/repo bookmark master_bookmark
  searching for changes
  updating bookmark master_bookmark
