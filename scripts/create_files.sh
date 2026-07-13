#! /usr/bin/env nix-shell
#! nix-shell -i bash -p bash


# CLI ARGS
LONG_ARGS=target-dir:,socket
SHORT_ARGS=t:,s
OPTS=$(getopt -a -n "Test File Creator" --options $SHORT_ARGS --longoptions $LONG_ARGS -- "$@")
eval set -- "$OPTS"

socket=false
while :
do
	case "$1" in
		-t | --target-dir )
		target_dir="${2}"
		shift 2;
		;;

    -s | --socket )
    socket=true
    echo "Unmounting and removing Folder"
    shift 2;
    ;;

		-- )
		shift;
		break
		;;

		* )
		echo "Unexpected option $1"
		exit 2
		;;
	esac
done

if [ -z "${target_dir:-}" ]
then
  echo "No Target-Dir Specified."
  exit 1
fi

echo "Target: ${target_dir}"
abs_tgt="$(readlink -m "${target_dir}")"
echo "Creating directory: ${abs_tgt}"
mkdir -p "${abs_tgt}"


if $socket
then
  nc -lU "${abs_tgt}/socket.sock" &
  echo "Created nc instance listening on socket.sock"
  exit 0
fi

echo "Creating File..."
touch "${abs_tgt}/file.txt"

echo "Creating Directory..."
mkdir -p "${abs_tgt}/dir/"

echo "Creating Symlink..."
ln -s "${abs_tgt}/file.txt" "${abs_tgt}/symlink"

# INFO: The socket was already created earlier.
echo ""
echo "The following Directory Entries need to root rights or CAP_MKNOD"
echo "If this failes, consider adding your user to CAP_MKNOD or invoking this script with root"
echo ""

echo "Creating block device"
mknod "${abs_tgt}/block_dev" b 99 99

echo "Create character device"
mknod "${abs_tgt}/block_dev" c 99 99

echo "Creating fifo queue"
mknod "${abs_tgt}/fifo" p

echo ""
echo "Done crating files"