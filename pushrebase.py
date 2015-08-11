# pushrebase.py - server-side rebasing of pushed commits
#
# Copyright 2014 Facebook, Inc.
#
# This software may be used and distributed according to the terms of the
# GNU General Public License version 2 or any later version.

import errno, os, tempfile, sys, operator, resource

try:
    import json
except ImportError:
    import simplejson as json

from mercurial import bundle2, cmdutil, hg, scmutil, exchange, commands
from mercurial import util, error, discovery, changegroup, context, revset
from mercurial import obsolete, pushkey, phases, extensions
from mercurial.extensions import wrapcommand, wrapfunction, _order
from mercurial.bundlerepo import bundlerepository
from mercurial.node import nullid, hex, bin
from mercurial.i18n import _

testedwith = '3.4'

cmdtable = {}
command = cmdutil.command(cmdtable)

rebaseparttype = 'b2x:rebase'
commonheadsparttype = 'b2x:commonheads'

experimental = 'experimental'
configonto = 'server-rebase-onto'

def uisetup(ui):
    # remotenames circumvents the default push implementation entirely, so make
    # sure we load after it so that we wrap it.
    _order.remove('pushrebase')
    _order.append('pushrebase')

def extsetup(ui):
    entry = wrapcommand(commands.table, 'push', _push)
    try:
        # Don't add the 'to' arg if it already exists
        extensions.find('remotenames')
    except KeyError:
        entry[1].append(('', 'to', '', _('server revision to rebase onto')))

    partorder = exchange.b2partsgenorder
    partorder.insert(partorder.index('changeset'),
                     partorder.pop(partorder.index(rebaseparttype)))

    partorder.insert(0, partorder.pop(partorder.index(commonheadsparttype)))

    wrapfunction(discovery, 'checkheads', _checkheads)

    origpushkeyhandler = bundle2.parthandlermapping['pushkey']
    newpushkeyhandler = lambda *args, **kwargs: \
        bundle2pushkey(origpushkeyhandler, *args, **kwargs)
    newpushkeyhandler.params = origpushkeyhandler.params
    bundle2.parthandlermapping['pushkey'] = newpushkeyhandler
    bundle2.parthandlermapping['b2x:pushkey'] = newpushkeyhandler

def validaterevset(repo, revset):
    "Abort if this is a rebasable revset, return None otherwise"
    if not repo.revs(revset):
        raise util.Abort(_('nothing to rebase'))

    if repo.revs('%r and public()', revset):
        raise util.Abort(_('cannot rebase public changesets'))

    if repo.revs('%r and obsolete()', revset):
        raise util.Abort(_('cannot rebase obsolete changesets'))

    heads = repo.revs('heads(%r)', revset)
    if len(heads) > 1:
        raise util.Abort(_('cannot rebase divergent changesets'))

    repo.ui.note(_('validated revset for rebase\n'))

def getrebasepart(repo, peer, outgoing, onto, newhead=False):
    if not outgoing.missing:
        raise util.Abort(_('no commits to rebase'))

    if rebaseparttype not in bundle2.bundle2caps(peer):
        raise util.Abort(_('no server support for %r') % rebaseparttype)

    validaterevset(repo, revset.formatspec('%ln', outgoing.missing))

    cg = changegroup.getlocalchangegroupraw(repo, 'push', outgoing)

    # .upper() marks this as a mandatory part: server will abort if there's no
    #  handler
    return bundle2.bundlepart(rebaseparttype.upper(),
                              mandatoryparams={'onto': onto,
                                               'newhead': repr(newhead),
                                              }.items(),
                              data = cg)

def _checkheads(orig, repo, remote, *args, **kwargs):
    onto = repo.ui.config(experimental, configonto)
    if onto: # This is a rebasing push
        # The rest of the checks are performed during bundle2 part processing;
        # we need to bypass the regular push checks because it will look like
        # we're pushing a new head, which isn't normally allowed
        if not repo.ui.configbool('experimental', 'bundle2-exp', False):
            raise util.Abort(_('bundle2 needs to be enabled on client'))
        if not remote.capable('bundle2-exp'):
            raise util.Abort(_('bundle2 needs to be enabled on server'))
        return
    else:
        return orig(repo, remote, *args, **kwargs)

def _push(orig, ui, repo, *args, **opts):
    oldonto = ui.backupconfig(experimental, configonto)
    oldremotenames = ui.backupconfig('remotenames', 'allownonfastforward')
    oldphasemove = None

    try:
        ui.setconfig(experimental, configonto, opts.get('to'), '--to')
        if ui.config(experimental, configonto):
            ui.setconfig(experimental, 'bundle2.pushback', True)
            oldphasemove = wrapfunction(exchange, '_localphasemove', _phasemove)
        ui.setconfig('remotenames', 'allownonfastforward', True)
        result = orig(ui, repo, *args, **opts)
    finally:
        ui.restoreconfig(oldonto)
        ui.restoreconfig(oldremotenames)
        if oldphasemove:
            exchange._localphasemove = oldphasemove

    return result

def _phasemove(orig, pushop, nodes, phase=phases.public):
    """prevent commits from being marked public

    Since these are going to be mutated on the server, they aren't really being
    published, their successors are.  If we mark these as public now, hg evolve
    will refuse to fix them for us later."""

    if phase != phases.public:
        orig(pushop, nodes, phase)

@exchange.b2partsgenerator(commonheadsparttype)
def commonheadspartgen(pushop, bundler):
    bundler.newpart(commonheadsparttype,
                    data=''.join(pushop.outgoing.commonheads))

@bundle2.parthandler(commonheadsparttype)
def commonheadshandler(op, inpart):
    nodeid = inpart.read(20)
    while len(nodeid) == 20:
        op.records.add(commonheadsparttype, nodeid)
        nodeid = inpart.read(20)
    assert not nodeid # data should split evenly into blocks of 20 bytes

@exchange.b2partsgenerator(rebaseparttype)
def partgen(pushop, bundler):
    onto = pushop.ui.config(experimental, configonto)
    if 'changesets' in pushop.stepsdone or not onto or pushop.force:
        return

    pushop.stepsdone.add('changesets')
    if not pushop.outgoing.missing:
        # It's important that this text match the text found in upstream
        # Mercurial, since some tools rely on this string to know if a push
        # succeeded despite not pushing commits.
        pushop.ui.status(_('no changes found\n'))
        pushop.cgresult = 0
        return

    rebasepart = getrebasepart(pushop.repo,
                               pushop.remote,
                               pushop.outgoing,
                               onto,
                               pushop.newbranch)

    bundler.addpart(rebasepart)

    def handlereply(op):
        # server either succeeds or aborts; no code to read
        pushop.cgresult = 1

    return handlereply

bundle2.capabilities[rebaseparttype] = ()

def _makebundlefile(part):
    """constructs a temporary bundle file

    part.data should be an uncompressed v1 changegroup"""

    fp = None
    fd, bundlefile = tempfile.mkstemp()
    try: # guards bundlefile
        try: # guards fp
            fp = os.fdopen(fd, 'wb')
            magic = 'HG10UN'
            fp.write(magic)
            data = part.read(resource.getpagesize() - len(magic))
            while data:
                fp.write(data)
                data = part.read(resource.getpagesize())
        finally:
            fp.close()
    except:
        try:
            os.unlink(bundlefile)
        except:
            # we would rather see the original exception
            pass
        raise

    return bundlefile

def _getrevs(bundle, onto):
    'extracts and validates the revs to be imported'
    validaterevset(bundle, 'bundle()')
    revs = [bundle[r] for r in bundle.revs('sort(bundle())')]
    onto = bundle[onto.hex()]
    # Fast forward update, no rebase needed
    if list(bundle.set('bundle() & %d::', onto.rev())):
        return revs, onto

    if revs:
        # We want to rebase the highest bundle root that is an ancestor of
        # `onto`.
        oldonto = list(bundle.set('max(parents(bundle()) - bundle() & ::%d)',
                                  onto.rev()))
        if not oldonto:
            # If there's no shared history, only allow the rebase if the
            # incoming changes are completely distinct.
            sharedparents = list(bundle.set('parents(bundle()) - bundle()'))
            if not sharedparents:
                return revs, bundle[nullid]
            raise util.Abort(_('pushed commits do not branch from an ancestor '
                               'of the desired destination %s' % onto.hex()))
        oldonto = oldonto[0]

        # Computes a list of all files that are in the changegroup, and diffs it
        # against all the files that changed between the old onto (ex: our old
        # bookmark location) and the new onto (ex: the server's actual bookmark
        # location). Since oldonto->onto is the distance of the rebase, this
        # should catch any conflicting changes.
        files = reduce(operator.or_, [set(rev.files()) for rev in revs], set())
        filematcher = scmutil.matchfiles(bundle, files)
        commonmanifest = oldonto.manifest().matches(filematcher)
        ontomanifest = onto.manifest().matches(filematcher)
        conflicts = ontomanifest.diff(commonmanifest).keys()
        if conflicts:
            raise util.Abort(_('conflicting changes in %r') % conflicts)

    return revs, oldonto

def _graft(repo, rev, mapping):
    '''duplicate changeset "rev" with parents from "mapping"'''
    oldp1 = rev.p1().node()
    oldp2 = rev.p2().node()
    newp1 = mapping.get(oldp1, oldp1)
    newp2 = mapping.get(oldp2, oldp2)
    m = rev.manifest()
    def getfilectx(repo, memctx, path):
        if path in m:
            fctx = rev[path]
            flags = fctx.flags()
            copied = fctx.renamed()
            if copied:
                copied = copied[0]
            return context.memfilectx(repo, fctx.path(), fctx.data(),
                              islink='l' in flags,
                              isexec='x' in flags,
                              copied=copied)
        else:
            return None


    # If the incoming commit has no parents, but requested a rebase,
    # allow it only for the first commit. The null/null commit will always
    # be the first commit since we only allow a nullid->nonnullid mapping if the
    # incoming commits are a completely distinct history (see `sharedparents` in
    # getrevs()), so there's no risk of commits with a single null parent
    # accidentally getting translated first.
    if oldp1 == nullid and oldp2 == nullid:
        if newp1 != nullid:
            newp2 = nullid
            del mapping[nullid]

    if oldp1 != nullid and oldp2 != nullid:
        # If it's a merge commit, Mercurial's rev.files() only returns the files
        # that are different from both p1 and p2, so it would not capture all of
        # the incoming changes from p2 (for instance, new files in p2). The fix
        # is to manually diff the rev manifest and it's p1 to get the list of
        # files that have changed. We only need to diff against p1, and not p2,
        # because Mercurial constructs new commits by applying our specified
        # files on top of a copy of the p1 manifest, so we only need the diff
        # against p1.
        files = rev.manifest().diff(repo[oldp1].manifest()).keys()
    else:
        files = rev.files()

    return context.memctx(repo,
                          [newp1, newp2],
                          rev.description(),
                          files,
                          getfilectx,
                          rev.user(),
                          rev.date(),
                          rev.extra(),
                         ).commit()

def _buildobsolete(replacements, oldrepo, newrepo):
    'adds obsolete markers in replacements if enabled in newrepo'
    if obsolete.isenabled(newrepo, obsolete.createmarkersopt):
        markers = [(oldrepo[oldrev], (newrepo[newrev],))
                   for oldrev, newrev in replacements.items()
                   if newrev != oldrev]

        obsolete.createmarkers(newrepo, markers)

def _addpushbackchangegroup(repo, reply, outgoing):
    '''adds changegroup part to reply containing revs from outgoing.missing'''
    cgversions = set(reply.capabilities.get('changegroup'))
    if not cgversions:
        cgversions.add('01')
    version = max(cgversions & set(changegroup.packermap.keys()))

    cg = changegroup.getlocalchangegroupraw(repo,
                                            'rebase:reply',
                                            outgoing,
                                            version = version)

    cgpart = reply.newpart('CHANGEGROUP', data = cg)
    if version != '01':
        cgpart.addparam('version', version)

def _addpushbackobsolete(repo, reply, newrevs):
    '''adds obsoletion markers to reply that are relevant to newrevs
    (if enabled)'''
    if (obsolete.isenabled(repo, obsolete.exchangeopt) and repo.obsstore):
        try:
            markers = repo.obsstore.relevantmarkers(newrevs)
            exchange.buildobsmarkerspart(reply, markers)
        except ValueError, exc:
            repo.ui.status(_("can't send obsolete markers: %s") % exc.message)

def _addpushbackparts(op, replacements):
    '''adds pushback to reply if supported by the client'''
    if (op.records[commonheadsparttype]
        and op.reply
        and 'pushback' in op.reply.capabilities):
        outgoing = discovery.outgoing(op.repo.changelog,
                                      op.records[commonheadsparttype],
                                      [new for old, new in replacements.items()
                                       if old != new])

        if outgoing.missing:
            _addpushbackchangegroup(op.repo, op.reply, outgoing)
            _addpushbackobsolete(op.repo, op.reply, replacements.values())

@bundle2.parthandler(rebaseparttype, ('onto', 'newhead'))
def bundle2rebase(op, part):
    '''unbundle a bundle2 containing a changegroup to rebase'''

    params = part.params
    tr = op.gettransaction()
    hookargs = dict(tr.hookargs)

    bundlefile = None
    try:
        onto = scmutil.revsingle(op.repo, params['onto'])
    except error.RepoLookupError:
        # Probably a new bookmark. onto == None means don't do rebasing
        onto = None

    if not params['newhead']:
        if not op.repo.revs('%r and head()', params['onto']):
            raise util.Abort(_('rebase would produce a new head on server'))

    try: # guards bundlefile
        bundlefile = _makebundlefile(part)
        bundle = bundlerepository(op.repo.ui, op.repo.root, bundlefile)
        if onto == None:
            maxcommonanc = list(bundle.set('max(parents(bundle()) - bundle())'))
            if not maxcommonanc:
                onto = op.repo[nullid]
            else:
                onto = maxcommonanc[0]

        revs, oldonto = _getrevs(bundle, onto)

        op.repo.hook("prechangegroup", **hookargs)

        mapping = {}

        # Seed the mapping with oldonto->onto
        mapping[oldonto.node()] = onto.node()

        replacements = {}
        added = []

        for rev in revs:
            newrev = _graft(op.repo, rev, mapping)

            new = op.repo[newrev]
            oldnode = rev.node()
            newnode = new.node()
            replacements[oldnode] = newnode
            mapping[oldnode] = newnode
            added.append(newnode)

        _buildobsolete(replacements, bundle, op.repo)
    finally:
        try:
            if bundlefile:
                os.unlink(bundlefile)
        except OSError, e:
            if e.errno != errno.ENOENT:
                raise

    publishing = op.repo.ui.configbool('phases', 'publish', True)
    if publishing:
        phases.advanceboundary(op.repo, tr, phases.public, [added[-1]])

    p = lambda: tr.writepending() and op.repo.root or ""
    op.repo.hook("pretxnchangegroup", throw=True, pending=p, **hookargs)

    def runhooks():
        args = hookargs.copy()
        args['node'] = hex(added[0])
        op.repo.hook("changegroup", **args)
        for n in added:
            args = hookargs.copy()
            args['node'] = hex(n)
            op.repo.hook("incoming", **args)

    tr.addpostclose('serverrebase-cg-hooks',
                    lambda tr: op.repo._afterlock(runhooks))

    _addpushbackparts(op, replacements)

    for k in replacements.keys():
        replacements[hex(k)] = hex(replacements[k])
    op.records.add(rebaseparttype, replacements)

    return 1

def bundle2pushkey(orig, op, part):
    replacements = dict(sum([record.items()
                             for record
                             in op.records[rebaseparttype]],
                            []))

    namespace = pushkey.decode(part.params['namespace'])
    if namespace == 'phases':
        key = pushkey.decode(part.params['key'])
        part.params['key'] = pushkey.encode(replacements.get(key, key))
    if namespace == 'bookmarks':
        new = pushkey.decode(part.params['new'])
        part.params['new'] = pushkey.encode(replacements.get(new, new))

    return orig(op, part)
