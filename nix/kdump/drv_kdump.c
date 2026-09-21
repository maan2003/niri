// SPDX-License-Identifier: GPL-2.0-only
/*
 * drv_kdump: the kernel's own account of one task, for the dev VM only. Everything the
 * forker sets that /proc does not show: securebits, MDWE, the Landlock domain with its rules
 * as paths, plus what /proc does show, read from the structs rather than formatted by it.
 * nix/kernel-state.sh diffs the output against nix/expect. Never on a real install.
 *
 *   echo <pid> > /sys/kernel/debug/drv/task; cat /sys/kernel/debug/drv/task
 *
 * Landlock's structs are private to security/landlock; the copies below are 6.18's.
 */
#include <linux/module.h>
#include <linux/debugfs.h>
#include <linux/seq_file.h>
#include <linux/kprobes.h>
#include <linux/lsm_hooks.h>
#include <linux/cred.h>
#include <linux/sched.h>
#include <linux/sched/mm.h>
#include <linux/sched/task.h>
#include <linux/nsproxy.h>
#include <linux/fs_struct.h>
#include <linux/dcache.h>
#include <linux/fs.h>
#include <linux/rbtree.h>
#include <linux/seccomp.h>
#include <linux/mm.h>
#include <linux/pid.h>
#include <linux/rcupdate.h>
#include <linux/user_namespace.h>
#include <linux/securebits.h>
#include <uapi/linux/landlock.h>

/* security/landlock/{access,object,ruleset,cred,domain}.h, v6.18, fields we read. */
typedef u16 access_mask_t;
struct access_masks {
	access_mask_t fs : 16;
	access_mask_t net : 2;
	access_mask_t scope : 2;
};
struct landlock_layer {
	u16 level;
	access_mask_t access;
};
struct landlock_object {
	refcount_t usage;
	spinlock_t lock;
	void *underobj;
	union {
		struct rcu_head rcu_free;
		const void *underops;
	};
};
union landlock_key {
	struct landlock_object *object;
	uintptr_t data;
};
struct landlock_rule {
	struct rb_node node;
	union landlock_key key;
	u32 num_layers;
	struct landlock_layer layers[];
};
struct landlock_hierarchy {
	struct landlock_hierarchy *parent;
	refcount_t usage;
#ifdef CONFIG_AUDIT
	int log_status;
	atomic64_t num_denials;
	u64 id;
	const void *details;
	u32 log_same_exec : 1, log_new_exec : 1;
#endif
};
struct landlock_ruleset {
	struct rb_root root_inode;
#if IS_ENABLED(CONFIG_INET)
	struct rb_root root_net_port;
#endif
	struct landlock_hierarchy *hierarchy;
	union {
		struct work_struct work_free;
		struct {
			struct mutex lock;
			refcount_t usage;
			u32 num_rules;
			u32 num_layers;
			struct access_masks access_masks[];
		};
	};
};
struct landlock_cred_security {
	struct landlock_ruleset *domain;
#ifdef CONFIG_AUDIT
	u16 domain_exec;
	u8 log_subdomains_off : 1;
#endif
} __packed;

static const char *const fs_names[] = {
	"execute", "write_file", "read_file", "read_dir", "remove_dir", "remove_file",
	"make_char", "make_dir", "make_reg", "make_sock", "make_fifo", "make_block",
	"make_sym", "refer", "truncate", "ioctl_dev",
};

static size_t landlock_cred_offset;
static pid_t wanted;

static void put_fs_mask(struct seq_file *m, u32 mask)
{
	int i;
	bool first = true;

	if (!mask) {
		seq_puts(m, "none");
		return;
	}
	for (i = 0; i < ARRAY_SIZE(fs_names); i++) {
		if (!(mask & BIT(i)))
			continue;
		seq_printf(m, "%s%s", first ? "" : ",", fs_names[i]);
		first = false;
	}
}

static void put_securebits(struct seq_file *m, unsigned int bits)
{
	static const char *const names[] = {
		"noroot", "noroot_locked", "no_setuid_fixup", "no_setuid_fixup_locked",
		"keep_caps", "keep_caps_locked", "no_cap_ambient_raise",
		"no_cap_ambient_raise_locked", "exec_restrict_file",
		"exec_restrict_file_locked", "exec_deny_interactive",
		"exec_deny_interactive_locked",
	};
	int i;

	seq_printf(m, "securebits 0x%x", bits);
	for (i = 0; i < ARRAY_SIZE(names); i++)
		if (bits & BIT(i))
			seq_printf(m, " %s", names[i]);
	seq_putc(m, '\n');
}

static void put_landlock(struct seq_file *m, const struct cred *c)
{
	const struct landlock_cred_security *lcs = c->security + landlock_cred_offset;
	const struct landlock_ruleset *dom = lcs->domain;
	struct rb_node *n;
	char *buf;
	u32 l;

	if (!dom) {
		seq_puts(m, "landlock none\n");
		return;
	}
	seq_printf(m, "landlock layers %u rules %u\n", dom->num_layers, dom->num_rules);
	for (l = 0; l < dom->num_layers; l++) {
		seq_printf(m, "landlock layer %u handles fs ", l);
		put_fs_mask(m, dom->access_masks[l].fs);
		seq_printf(m, " net 0x%x scope 0x%x\n", dom->access_masks[l].net,
			   dom->access_masks[l].scope);
	}
	buf = kmalloc(PATH_MAX, GFP_KERNEL);
	if (!buf)
		return;
	for (n = rb_first(&dom->root_inode); n; n = rb_next(n)) {
		const struct landlock_rule *rule = rb_entry(n, struct landlock_rule, node);
		struct inode *inode = rule->key.object->underobj;
		struct dentry *d;
		const char *path = "(released)";

		if (inode) {
			d = d_find_any_alias(inode);
			if (d) {
				path = dentry_path_raw(d, buf, PATH_MAX);
				if (IS_ERR(path))
					path = "(toolong)";
			} else {
				path = "(noalias)";
			}
			seq_printf(m, "rule %s:%s ino %lu %s", inode->i_sb->s_type->name,
				   inode->i_sb->s_id, inode->i_ino, path);
			if (d)
				dput(d);
		} else {
			seq_printf(m, "rule %s", path);
		}
		for (l = 0; l < rule->num_layers; l++) {
			seq_printf(m, " layer%u=", rule->layers[l].level);
			put_fs_mask(m, rule->layers[l].access);
		}
		seq_putc(m, '\n');
	}
	kfree(buf);
}

static int task_show(struct seq_file *m, void *v)
{
	struct task_struct *t;
	const struct cred *c;
	struct mm_struct *mm;
	struct nsproxy *ns, *ins;
	struct path root;
	char *buf;
	int i;

	rcu_read_lock();
	t = pid_task(find_vpid(wanted), PIDTYPE_PID);
	if (t)
		get_task_struct(t);
	rcu_read_unlock();
	if (!t) {
		seq_printf(m, "no task %d\n", wanted);
		return 0;
	}
	rcu_read_lock();
	seq_printf(m, "task %d %s parent %s\n", wanted, t->comm,
		   rcu_dereference(t->real_parent)->comm);
	rcu_read_unlock();

	c = get_task_cred(t);
	seq_printf(m, "uid %u %u %u %u gid %u %u %u %u groups",
		   from_kuid(&init_user_ns, c->uid), from_kuid(&init_user_ns, c->euid),
		   from_kuid(&init_user_ns, c->suid), from_kuid(&init_user_ns, c->fsuid),
		   from_kgid(&init_user_ns, c->gid), from_kgid(&init_user_ns, c->egid),
		   from_kgid(&init_user_ns, c->sgid), from_kgid(&init_user_ns, c->fsgid));
	for (i = 0; i < c->group_info->ngroups; i++)
		seq_printf(m, " %u", from_kgid(&init_user_ns, c->group_info->gid[i]));
	seq_putc(m, '\n');
	seq_printf(m, "caps inh %016llx prm %016llx eff %016llx bnd %016llx amb %016llx\n",
		   c->cap_inheritable.val, c->cap_permitted.val, c->cap_effective.val,
		   c->cap_bset.val, c->cap_ambient.val);
	put_securebits(m, c->securebits);
	seq_printf(m, "user_ns %s\n", c->user_ns == &init_user_ns ? "host" : "own");
	put_landlock(m, c);
	put_cred(c);

	seq_printf(m, "no_new_privs %d\n", task_no_new_privs(t) ? 1 : 0);
	seq_printf(m, "seccomp mode %d filters %d\n", t->seccomp.mode,
		   atomic_read(&t->seccomp.filter_count));

	mm = get_task_mm(t);
	if (mm) {
		seq_printf(m, "mdwe %d no_inherit %d\n",
			   mm_flags_test(MMF_HAS_MDWE, mm) ? 1 : 0,
			   mm_flags_test(MMF_HAS_MDWE_NO_INHERIT, mm) ? 1 : 0);
		mmput(mm);
	}

	task_lock(t);
	ns = t->nsproxy;
	ins = init_task.nsproxy;
	if (ns)
		seq_printf(m, "ns mnt %s net %s pid %s uts %s ipc %s cgroup %s time %s\n",
			   ns->mnt_ns == ins->mnt_ns ? "host" : "own",
			   ns->net_ns == ins->net_ns ? "host" : "own",
			   ns->pid_ns_for_children == ins->pid_ns_for_children ? "host" : "own",
			   ns->uts_ns == ins->uts_ns ? "host" : "own",
			   ns->ipc_ns == ins->ipc_ns ? "host" : "own",
			   ns->cgroup_ns == ins->cgroup_ns ? "host" : "own",
			   ns->time_ns == ins->time_ns ? "host" : "own");
	task_unlock(t);

	buf = kmalloc(PATH_MAX, GFP_KERNEL);
	if (buf && t->fs) {
		get_fs_root(t->fs, &root);
		seq_printf(m, "root %s:%s %s\n", root.mnt->mnt_sb->s_type->name,
			   root.mnt->mnt_sb->s_id,
			   root.dentry == root.mnt->mnt_root ? "mount-root" : "subdir");
		path_put(&root);
	}
	kfree(buf);
	put_task_struct(t);
	return 0;
}

static int task_open(struct inode *inode, struct file *file)
{
	return single_open(file, task_show, NULL);
}

static ssize_t task_write(struct file *file, const char __user *ubuf, size_t len, loff_t *off)
{
	char kbuf[16];
	int pid;

	if (len >= sizeof(kbuf))
		return -EINVAL;
	if (copy_from_user(kbuf, ubuf, len))
		return -EFAULT;
	kbuf[len] = 0;
	if (kstrtoint(strim(kbuf), 10, &pid))
		return -EINVAL;
	wanted = pid;
	return len;
}

static const struct file_operations task_fops = {
	.owner = THIS_MODULE,
	.open = task_open,
	.read = seq_read,
	.write = task_write,
	.llseek = seq_lseek,
	.release = single_release,
};

static struct dentry *dir;

static int __init kdump_init(void)
{
	/* landlock_blob_sizes is not exported: find it the way every debugging module does. */
	struct kprobe kp = { .symbol_name = "kallsyms_lookup_name" };
	unsigned long (*lookup)(const char *);
	const struct lsm_blob_sizes *sizes;
	int ret;

	ret = register_kprobe(&kp);
	if (ret)
		return ret;
	lookup = (void *)kp.addr;
	unregister_kprobe(&kp);
	sizes = (void *)lookup("landlock_blob_sizes");
	if (!sizes)
		return -ENOENT;
	landlock_cred_offset = sizes->lbs_cred;
	dir = debugfs_create_dir("drv", NULL);
	debugfs_create_file("task", 0600, dir, NULL, &task_fops);
	pr_info("drv_kdump: landlock cred offset %zu\n", landlock_cred_offset);
	return 0;
}

static void __exit kdump_exit(void)
{
	debugfs_remove_recursive(dir);
}

module_init(kdump_init);
module_exit(kdump_exit);
MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("drv dev VM: dump a task's kernel-side state");
