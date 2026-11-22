
get_words() {
  case $1 in
    1) echo "one" ;;
    2) echo "a couple" ;;
    3) echo "a few" ;;
    *) echo "lots" ;;
  esac
}
