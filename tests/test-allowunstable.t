Set up test environment.
  $ . $TESTDIR/require-ext.sh directaccess evolve inhibit
  $ extpath=`dirname $TESTDIR`
  $ cp $extpath/hgext3rd/allowunstable.py $TESTTMP # use $TESTTMP substitution in message
  $ cat >> $HGRCPATH << EOF
  > [extensions]
  > allowunstable=$TESTTMP/allowunstable.py
  > directaccess=
  > evolve=
  > histedit=
  > inhibit=
  > [experimental]
  > evolution = createmarkers
  > evolutioncommands = fold split
  > EOF
  $ showgraph() {
  >   hg log --graph -T "{rev} {desc|firstline}"
  > }
  $ hg init allowunstable && cd allowunstable
  $ hg debugbuilddag +4

Test that we can perform a splits and histedits in the middle of a stack.
Since these are interactive commands, just ensure that we don't get
an error message.
  $ hg up 2
  0 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ hg split
  0 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ hg histedit

Test that we can perform a fold in the middle of a stack.
  $ hg up 2
  0 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ hg fold ".^"
  2 changesets folded
  0 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ showgraph
  @  4 r1
  |
  | o  3 r3
  | |
  | o  2 r2
  | |
  | o  1 r1
  |/
  o  0 r0
  

