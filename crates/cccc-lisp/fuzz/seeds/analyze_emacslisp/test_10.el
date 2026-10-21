
            (defun host (xs)
              (cl-flet ((helper (x) (if x 1 0)))
                (mapcar #'helper xs)))
        