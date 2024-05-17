unset fault
  CLIPPY_FLAGS="-W clippy::default_trait_access -W clippy::manual_string_new -W clippy::cognitive_complexity -W clippy::implicit_clone -W clippy::range-plus-one -W clippy::redundant-clone -W clippy::match_bool"
  fault_type="error"
  fault_prefix=$(echo "$fault_type" | tr '[:lower:]' '[:upper:]')
  # * convert any warnings to GHA UI annotations; ref: <https://help.github.com/en/actions/reference/workflow-commands-for-github-actions#setting-a-warning-message>
  S=$(cargo clippy --all-targets --features feat_os_unix -pcoreutils -- ${CLIPPY_FLAGS} -D warnings 2>&1) && printf "%s\n" "$S" || { printf "%s\n" "$S" ; printf "%s" "$S" | sed -E -n -e '/^error:/{' -e "N; s/^error:[[:space:]]+(.*)\\n[[:space:]]+-->[[:space:]]+(.*):([0-9]+):([0-9]+).*$/::${fault_type} file=\2,line=\3,col=\4::${fault_prefix}: \`cargo clippy\`: \1 (file:'\2', line:\3)/p;" -e '}' ; fault=true ; }
  if [ -n "true" ] && [ -n "$fault" ]; then exit 1 ; fi