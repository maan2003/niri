# The SELinux spike (dev VM only; drv's specs/NOTES-selinux.md has the findings): a policy for this kernel from the kernel's own dummy-policy
# generator (scripts/selinux/mdp: every class and permission the kernel knows, one type `base_t`
# allowed everything, the initial sids, fs_use and genfscon lines), with drv's types on top:
#   drv_app_t  the domain apps run in (the forker transitions into it before the exec);
#   door_t     a door apps may not reach (nothing lets drv_app_t near it).
# Compiled with checkpolicy into what /etc/selinux/drv needs: the binary policy and a
# file_contexts that labels everything base_t.
{ pkgs, kernel }:
let
  mdp = pkgs.runCommandCC "mdp-${kernel.version}" { } ''
    tar xf ${kernel.src} --wildcards '*/scripts/selinux/mdp/mdp.c' '*/security/selinux/include/*' '*/include/*'
    cd linux-*
    cc -o $out -Iinclude -Isecurity/selinux/include \
      -I${kernel.dev}/lib/modules/${kernel.modDirVersion}/build/include scripts/selinux/mdp/mdp.c
  '';
  drv = pkgs.writeText "drv.te" ''
    type drv_app_t;
    type door_t;
    role base_r types { drv_app_t };
    # A door's label may sit on the filesystem (tmpfs) its socket is in.
    allow door_t base_t:filesystem associate;
  '';
in
pkgs.runCommand "drv-selinux-policy" { nativeBuildInputs = [ pkgs.checkpolicy ]; } ''
  ${mdp} mdp.conf file_contexts
  # policy.conf is ordered: the type enforcement rules go before the users and contexts.
  sed -n '1,/^user /{/^user /!p}' mdp.conf > policy.conf
  cat ${drv} >> policy.conf
  # Every class: an app may do anything to itself and to the rest of the system (base_t), the
  # system anything to it and to the doors. Nothing for drv_app_t on door_t.
  for c in $(grep '^class ' mdp.conf | awk '{print $2}' | sort -u); do
    echo "allow drv_app_t self:$c *;"
    echo "allow drv_app_t base_t:$c *;"
    echo "allow base_t drv_app_t:$c *;"
    echo "allow base_t door_t:$c *;"
  done >> policy.conf
  sed -n '/^user /,$p' mdp.conf >> policy.conf
  vers=$(checkpolicy -V | cut -d' ' -f1)
  mkdir -p $out/policy $out/contexts/files
  checkpolicy -U allow -o $out/policy/policy.$vers policy.conf
  cp policy.conf $out/policy.conf
  cp file_contexts $out/contexts/files/file_contexts
''
