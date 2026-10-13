
            (defun safe (thunk)
              (condition-case err
                  (funcall thunk)
                (error
                 (if (recoverable-p err) (retry) (abort)))))
        